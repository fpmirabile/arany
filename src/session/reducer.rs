use super::events::{
    AgentDisposition, AgentRole, AgentRunId, CompactionRecord, Event, EventEnvelope, ForkLineage,
    GoalBudget, GoalId, GoalPhase, GoalPlanContract, GoalReport, GoalStatus, GoalUsage,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition, RunId,
    SessionDefaults, SessionId,
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
    pub workspace_identity: (u64, u64),
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
    /// Run and reserved milliseconds of the unsettled active-time segment.
    pub reserved_active_run: Option<RunId>,
    pub reserved_active_ms: Option<u64>,
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
}

impl GoalView {
    pub(crate) fn effective_budget(&self) -> GoalBudget {
        self.budget.effective(self.plan_contract.as_ref())
    }

    /// Whether the settled usage of this Goal admits another Run under its effective budget.
    pub(crate) fn admits_next_run(&self) -> bool {
        self.effective_budget().admits_next_run(GoalUsage {
            runs: self.run_ids.len(),
            provider_calls: self.provider_calls,
            active_ms: self.active_ms,
            known_tokens: self.known_tokens,
            token_usage_unknown: self.token_usage_unknown,
        })
    }

    /// Usage that decides whether an explicit resume admits another Run. Resume settles an unsettled
    /// active-time reservation as spent and counts unobserved calls as unknown tokens; an unaccepted
    /// trailing link is not a Run. Saturation fails admission closed.
    pub(crate) fn resume_usage(&self, view: &SessionView) -> GoalUsage {
        let unaccepted_link = self
            .run_ids
            .last()
            .is_some_and(|run_id| !view.runs.iter().any(|run| run.id == *run_id));
        let reserved_ms = self.reserved_active_ms.unwrap_or_default();
        GoalUsage {
            runs: self.run_ids.len() - usize::from(unaccepted_link),
            provider_calls: self.provider_calls,
            active_ms: self.active_ms.saturating_add(reserved_ms),
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
mod goal_recovery_tests {
    use super::*;
    use crate::tools::{EffectIntent, ToolCall, ToolDisposition, ToolObservation};

    fn pending_plan_view(disposition: Option<ToolDisposition>) -> SessionView {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let run_id = RunId::new();
        let intent = EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 32,
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let tool = ToolView {
            intent: intent.clone(),
            observation: disposition.map(|disposition| ToolObservation {
                intent,
                disposition,
                output: if disposition == ToolDisposition::Succeeded {
                    "{}".into()
                } else {
                    String::new()
                },
                guard: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            finished_sequence: disposition.map(|_| 2),
        };
        SessionView {
            id: session_id,
            title: "Interrupted Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some((1, 2)),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
                id: run_id,
                objective: "Inspect before planning".into(),
                images: Vec::new(),
                config: None,
                agents: Vec::new(),
                assistant_message: None,
                status: RunStatus::Interrupted,
                accepted_sequence: 1,
                finished_sequence: None,
                tools: vec![tool],
                tool_compaction_summary: None,
                tool_compaction_covered_count: 0,
                display: Vec::new(),
                steering_notes: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: "Inspect before planning".into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                resource_settings: crate::tools::ToolResourceSettings::default(),
                phase: GoalPhase::Research,
                autonomous: false,
                status: GoalStatus::Interrupted,
                run_ids: vec![run_id],
                active_run: None,
                plan_contract: None,
                success_state: crate::session::GoalSuccessState::NoContract,
                success_criterion_satisfied: false,
                awaiting_plan_contract: true,
                active_ms: 0,
                uncertain_active_ms: 0,
                reserved_active_run: None,
                reserved_active_ms: None,
                provider_calls: 1,
                unobserved_provider_calls: Vec::new(),
                compaction_call_reserved: None,
                uncertain_compaction_calls: 0,
                known_tokens: 0,
                known_cached_tokens: 0,
                token_usage_unknown: false,
                report: None,
                started_sequence: 1,
                finished_sequence: None,
            }],
        }
    }

    #[test]
    fn pending_plan_recovery_accepts_closed_inspection_and_refuses_open_or_uncertain_tools() {
        let closed = pending_plan_view(Some(ToolDisposition::Succeeded));
        assert!(closed.goals[0].can_resume(&closed));

        let mut started_only = closed.clone();
        started_only.runs.clear();
        started_only.goals[0].run_ids.clear();
        started_only.goals[0].awaiting_plan_contract = false;
        assert!(started_only.goals[0].can_resume(&started_only));

        let open = pending_plan_view(None);
        assert!(!open.goals[0].can_resume(&open));

        let uncertain = pending_plan_view(Some(ToolDisposition::Uncertain));
        assert!(!uncertain.goals[0].can_resume(&uncertain));
    }
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
    pub finished_sequence: Option<u64>,
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
        .filter(|tool| !is_integration_tool(tool))
        .count()
}

fn is_integration_tool(tool: &ToolView) -> bool {
    matches!(
        tool.intent.call,
        crate::tools::ToolCall::ApplyChangeset { .. }
    )
}

/// Model Tool requests declared by an AgentRun's ToolRequested calls.
fn requested_tool_total(agent: &AgentView) -> usize {
    agent
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .map(|call| usize::from(call.requested_tools))
        .sum()
}

/// Declared model Tool requests whose observation has not been journaled yet.
fn pending_tool_requests(run: &RunView) -> Option<usize> {
    requested_tool_total(run.primary()?).checked_sub(model_tool_count(run))
}

/// A covered prefix may not end inside a step: its model Tool count must be a cumulative step total.
fn ends_whole_step(run: &RunView, covered_count: usize) -> bool {
    let Some(primary) = run.primary() else {
        return false;
    };
    let covered = run.tools[..covered_count]
        .iter()
        .filter(|tool| !is_integration_tool(tool))
        .count();
    let step_ends = primary
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .scan(0, |total, call| {
            *total += usize::from(call.requested_tools);
            Some(*total)
        });
    step_ends.into_iter().any(|end| end == covered)
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

fn valid_integration_source(run: &RunView, intent: &crate::tools::EffectIntent) -> bool {
    let crate::tools::ToolCall::ApplyChangeset {
        source_intent_digest,
        changeset,
    } = &intent.call
    else {
        return false;
    };
    let Some(primary) = run.primary() else {
        return false;
    };
    if primary.provider_calls.last().is_none_or(|call| {
        call.disposition != ProviderCallDisposition::ToolRequested
            && call.phase != AgentPhase::ToolReview
    }) || pending_tool_requests(run) != Some(0)
    {
        return false;
    }
    run.tools
        .last()
        .and_then(|tool| tool.observation.as_ref())
        .is_some_and(|observation| {
            observation.intent.digest() == *source_intent_digest
                && observed_command_changeset(observation)
                    .is_some_and(|source| source == *changeset)
        })
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

fn is_tool_review(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Primary
        && record.phase == AgentPhase::ToolReview
        && agent.provider_calls.last().is_some_and(|call| {
            call.disposition == ProviderCallDisposition::ToolRequested
                || call.phase == AgentPhase::ToolReview
                    && run.tools.last().is_some_and(|tool| {
                        matches!(
                            tool.intent.call,
                            crate::tools::ToolCall::ApplyChangeset { .. }
                        )
                    })
        })
        && run
            .tools
            .last()
            .is_some_and(|tool| tool.observation.is_none() && tool.review_call_index.is_none())
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionView {
    pub event_sequence: u64,
    pub record: CompactionRecord,
}

impl RunView {
    /// The objective, then the steering notes the user sent during this Run, as labelled user text.
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
                        runs: Vec::new(),
                        compactions: Vec::new(),
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
            && !matches!(
                &envelope.event,
                Event::GoalResumed { goal_id } if *goal_id == awaiting
            )
            && !matches!(&envelope.event, Event::GoalStarted { .. })
            && !matches!(
                &envelope.event,
                Event::GoalActiveTimeReserved { goal_id, .. } | Event::GoalActiveTimeSettled { goal_id, .. }
                    if *goal_id == awaiting
            )
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
            | Event::GoalActiveTimeReserved { .. }
            | Event::GoalActiveTimeSettled { .. }
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
            | Event::ProviderCallRecorded { .. }
            | Event::AgentFinished { .. } => agent::apply(current, &envelope.event)?,
            Event::ToolStarted { .. } | Event::ToolFinished { .. } => {
                tool::apply(current, &envelope.event, prior)?
            }
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

    /// The most recent unfinished Goal; explicit resume continues only this one, and older paused Goals stay history.
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

/// A failed or cancelled end of the Run is valid while a plan wait is open: the engine
/// ends the Run when a Tool is refused before `ToolStarted`, and that refusal leaves no
/// pending plan to record.
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

#[cfg(test)]
mod goal_replay_tests {
    use super::*;
    use crate::provider::{AgentPhase, OutputTokenBound};
    use crate::session::events::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
    use crate::tools::{
        CacheUpdate, EffectIntent, GuardReceipt, ToolCall, ToolDisposition, ToolLimits,
        ToolObservation, ToolPolicyReceipt,
    };

    const GOAL_OBJECTIVE: &str = "Goal";

    fn config(policy: CollaborationPolicy, tools: bool) -> RunConfig {
        RunConfig {
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            policy,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device: 1,
            workspace_inode: 2,
            instruction_digest: None,
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: None,
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: tools.then_some(ToolPolicyReceipt {
                contract_version: 1,
                config_digest: [1; 32],
                max_context_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES as u32,
            }),
        }
    }

    fn record(
        phase: AgentPhase,
        disposition: ProviderCallDisposition,
        tokens: Option<u32>,
    ) -> ProviderCallRecord {
        ProviderCallRecord {
            phase,
            disposition,
            response_id: None,
            input_tokens: tokens,
            output_tokens: tokens,
            wire_provenance: None,
            failure_reason: None,
            requested_tools: u8::from(disposition == ProviderCallDisposition::ToolRequested),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
    }

    fn journal(session_id: SessionId, events: Vec<Event>) -> Vec<EventEnvelope> {
        events
            .into_iter()
            .zip(1..)
            .map(|(event, sequence)| {
                let (run_id, agent_run_id) = event.scope();
                EventEnvelope {
                    sequence,
                    session_id,
                    run_id,
                    agent_run_id,
                    event,
                    created_at_ms: 0,
                }
            })
            .collect()
    }

    fn goal_started(goal_id: GoalId, budget: GoalBudget) -> Event {
        Event::GoalStarted {
            goal_id,
            objective: GOAL_OBJECTIVE.into(),
            workspace_identity: (1, 2),
            budget,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
        }
    }

    /// Links one Run, accepts its message and spawns its primary AgentRun.
    fn start_run(
        goal_id: GoalId,
        run_id: RunId,
        agent_run_id: AgentRunId,
        config: RunConfig,
    ) -> Vec<Event> {
        vec![
            Event::GoalRunLinked {
                goal_id,
                run_id,
                phase: GoalPhase::Implementation,
            },
            Event::MessageAccepted {
                run_id,
                text: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
            },
            Event::RunStarted { run_id, config },
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                grant: None,
            },
        ]
    }

    /// Completes the Run that `start_run` opened, and with it the Goal.
    fn complete_run_and_goal(
        goal_id: GoalId,
        run_id: RunId,
        agent_run_id: AgentRunId,
    ) -> Vec<Event> {
        vec![
            Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            },
            Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Finished,
                    None,
                ),
            },
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition: AgentDisposition::Finished,
                summary: Some("done".into()),
                result: Some("done".into()),
            },
            Event::MessageCommitted {
                run_id,
                text: "done".into(),
            },
            Event::RunFinished {
                run_id,
                disposition: RunDisposition::Finished,
            },
            Event::GoalFinished {
                goal_id,
                status: GoalStatus::Completed,
                active_ms: 0,
                report: GoalReport {
                    summary: "done".into(),
                    changes: Vec::new(),
                    evidence: Vec::new(),
                    unresolved: Vec::new(),
                    cached_input_tokens: 0,
                },
            },
        ]
    }

    /// Cancels the Run that `start_run` opened; its Goal stays open and paused.
    fn cancel_run(run_id: RunId, agent_run_id: AgentRunId) -> Vec<Event> {
        vec![
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition: AgentDisposition::Cancelled,
                summary: None,
                result: None,
            },
            Event::RunFinished {
                run_id,
                disposition: RunDisposition::Cancelled,
            },
        ]
    }

    fn session_with(session_id: SessionId, events: Vec<Event>) -> Vec<EventEnvelope> {
        let mut all = vec![Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: (1, 2),
        }];
        all.extend(events);
        journal(session_id, all)
    }

    #[test]
    fn steering_note_is_accepted_only_inside_an_open_primary_reservation() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let started = || {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(CollaborationPolicy::Single, false),
            ));
            events
        };
        let open = || {
            let mut events = started();
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            });
            events
        };
        let note = |goal_id: GoalId, text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        let accepts = |events: Vec<Event>| {
            SessionView::replay_open(session, &session_with(session, events)).is_ok()
        };

        let mut accepted = open();
        accepted.push(note(goal_id, "use the smaller fixture"));
        let view = SessionView::replay_open(session, &session_with(session, accepted.clone()))
            .expect("a note inside the open reservation replays")
            .expect("Session exists");
        assert_eq!(view.runs[0].steering_notes, vec!["use the smaller fixture"]);

        // Every rejected case first proves its prefix is accepted, so the rejection is not vacuous.
        let prefix = started();
        assert!(accepts(prefix.clone()));
        let mut before = prefix;
        before.push(note(goal_id, "before the reservation"));
        assert!(!accepts(before));

        let recorded_prefix = {
            let mut events = open();
            events.push(Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Finished,
                    None,
                ),
            });
            events
        };
        assert!(accepts(recorded_prefix.clone()));
        let mut recorded = recorded_prefix;
        recorded.push(note(goal_id, "after the call settled"));
        assert!(!accepts(recorded));

        let child = AgentRunId::new();
        let child_prefix = {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(
                    CollaborationPolicy::Auto {
                        max_active_children: 1,
                    },
                    false,
                ),
            ));
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            });
            events.push(Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Delegated,
                    None,
                ),
            });
            events.push(Event::AgentSpawned {
                run_id,
                agent_run_id: child,
                role: AgentRole::Child,
                ordinal: 1,
                objective: Some("inspect the parser".into()),
                grant: Some(ChildGrant::NoTools),
            });
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id: child,
            });
            events
        };
        assert!(accepts(child_prefix.clone()));
        let mut child_open = child_prefix;
        child_open.push(note(goal_id, "for the child"));
        assert!(!accepts(child_open));

        let mut full = open();
        full.extend((1..=4).map(|n| note(goal_id, &n.to_string())));
        assert!(accepts(full.clone()));
        full.push(note(
            goal_id,
            &"f".repeat(crate::session::MAX_STEERING_NOTE_BYTES),
        ));
        assert!(!accepts(full), "a Run's notes stay within the byte cap");

        // Each note charges its text plus its label, so exactly the per-Run byte cap is accepted.
        let label = crate::session::MAX_STEERING_LABEL_BYTES;
        let mut exact = open();
        exact.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
        exact.push(note(goal_id, &"b".repeat(4 * 1024 - label)));
        assert!(accepts(exact.clone()));
        let mut oversized = open();
        oversized.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
        oversized.push(note(goal_id, &"b".repeat(4 * 1024 - label + 1)));
        assert!(!accepts(oversized));

        let mut finished_prefix = open();
        finished_prefix.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        finished_prefix.push(Event::AgentFinished {
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        });
        assert!(accepts(finished_prefix.clone()));
        let mut between = finished_prefix;
        between.push(note(goal_id, "between AgentFinished and RunFinished"));
        assert!(!accepts(between));

        let mut ended_prefix = open();
        ended_prefix.extend(cancel_run(run_id, agent_run_id));
        assert!(accepts(ended_prefix.clone()));
        let mut finished = ended_prefix;
        finished.push(note(goal_id, "after the Run ended"));
        assert!(!accepts(finished));

        let other_prefix = open();
        assert!(accepts(other_prefix.clone()));
        let mut other = other_prefix;
        other.push(note(GoalId::new(), "another Goal"));
        assert!(!accepts(other));

        // A Tool review reserves under the primary id; its pending Tool observation must reject notes.
        let mut review = SessionView::replay_open(session, &session_with(session, open()))
            .expect("open reservation replays")
            .expect("Session exists");
        let intent = crate::tools::EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: crate::tools::ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 32,
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        review.runs[0].tools.push(ToolView {
            intent,
            observation: None,
            review_call_index: None,
            finished_sequence: None,
        });
        assert!(
            review
                .record_goal_steering_note(&goal_id, &run_id, "during a Tool review")
                .is_err()
        );
    }

    #[test]
    fn steering_notes_charge_labels_and_stay_within_the_run_byte_bound() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        let note = |text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        // One byte of text plus its label is the densest packing the bound allows.
        let most = crate::session::MAX_STEERING_NOTE_BYTES
            / (1 + crate::session::MAX_STEERING_LABEL_BYTES);
        events.extend((0..most).map(|_| note("n")));
        let view = SessionView::replay_open(session, &session_with(session, events.clone()))
            .expect("the maximum tiny notes replay")
            .expect("Session exists");
        let run = &view.runs[0];
        assert_eq!(run.steering_notes.len(), most);
        let text = run.history_user_text();
        assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
        events.push(note("n"));
        assert!(
            SessionView::replay_open(session, &session_with(session, events)).is_err(),
            "the next tiny note exceeds the Run's bound with its label"
        );
    }

    #[test]
    fn steering_notes_have_no_count_cap_but_keep_the_run_byte_bound() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let open = || {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(CollaborationPolicy::Single, false),
            ));
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            });
            events
        };
        let note = |len: usize| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: "n".repeat(len),
        };
        let mut events = open();
        events.extend((0..9).map(|_| note(800)));
        let view = SessionView::replay_open(session, &session_with(session, events.clone()))
            .expect("nine notes within 8 KiB replay")
            .expect("Session exists");
        assert_eq!(view.runs[0].steering_notes.len(), 9);
        // Nine notes of 800 bytes charge 9 x (800 + 8) = 7272; a 912-byte tenth charges 920 and
        // reaches exactly 8192, while 913 bytes would charge 921 and reach 8193.
        events.push(note(912));
        assert!(
            SessionView::replay_open(session, &session_with(session, events.clone())).is_ok(),
            "the Run's notes together reach exactly 8 KiB with labels"
        );
        events.push(note(913));
        assert!(
            SessionView::replay_open(session, &session_with(session, events)).is_err(),
            "one more byte exceeds the Run's bound"
        );
    }

    #[test]
    fn history_user_text_labels_notes_within_the_steering_reserve() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        for _ in 0..4 {
            events.push(Event::GoalSteeringNoteRecorded {
                goal_id,
                run_id,
                text: "n".repeat(2000),
            });
        }
        let view = SessionView::replay_open(session, &session_with(session, events))
            .expect("notes replay")
            .expect("Session exists");
        let run = &view.runs[0];
        let text = run.history_user_text();
        assert!(text.starts_with(&run.objective));
        assert!(text.contains("\n[4] nnnn"));
        // The reserve covers every note byte plus its labels and header, so compaction and history agree on size.
        assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
        let mut bare = run.clone();
        bare.steering_notes.clear();
        assert_eq!(bare.history_user_text(), run.objective);
    }

    #[test]
    fn crash_after_steering_note_replays_interrupted_and_resume_starts_without_notes() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        events.push(Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: "steer".into(),
        });
        // The process dies before the reserved call's outcome commits.
        let crashed = SessionView::replay(session, &session_with(session, events.clone()))
            .expect("crashed journal replays")
            .expect("Session exists");
        assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
        assert_eq!(crashed.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(crashed.runs[0].steering_notes, vec!["steer"]);

        let resumed_run = RunId::new();
        let resumed_agent = AgentRunId::new();
        events.push(Event::GoalResumed { goal_id });
        events.extend(start_run(
            goal_id,
            resumed_run,
            resumed_agent,
            config(CollaborationPolicy::Single, false),
        ));
        let resumed = SessionView::replay_open(session, &session_with(session, events))
            .expect("resume replays")
            .expect("Session exists");
        assert_eq!(resumed.runs.len(), 2);
        assert_eq!(resumed.runs[0].steering_notes, vec!["steer"]);
        assert!(resumed.runs[1].steering_notes.is_empty());
    }

    #[test]
    fn a_new_goal_is_accepted_after_a_paused_goal_and_owns_its_run() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(first_run, first_agent));
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(second, second_run, second_agent));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let open = SessionView::replay_open(session, &journal)
            .expect("a new Goal pauses its paused predecessor")
            .expect("Session exists");
        assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(open.goals[1].status, GoalStatus::Completed);
        assert_eq!(open.goals[1].run_ids, vec![second_run]);
        assert_eq!(open.goals[0].run_ids, vec![first_run]);
        let closed = SessionView::replay(session, &journal)
            .expect("closed replay agrees")
            .expect("Session exists");
        assert_eq!(closed.interrupted_goal_id(), Some(first));
    }

    #[test]
    fn a_crashed_predecessor_is_sealed_by_a_new_goal() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        // No terminal Run or Goal event: the process died mid-Run.
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let open = SessionView::replay_open(session, &journal)
            .expect("a crashed predecessor does not block a new Goal")
            .expect("Session exists");
        assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(open.runs[0].status, RunStatus::Interrupted);
        assert_eq!(open.goals[1].status, GoalStatus::Running);
    }

    #[test]
    fn resume_continues_the_most_recent_paused_goal_after_a_newer_goal_finished() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let (resumed_run, resumed_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(first_run, first_agent));
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(second, second_run, second_agent));
        events.push(Event::GoalResumed { goal_id: first });
        events.extend(start_run(
            first,
            resumed_run,
            resumed_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(first, resumed_run, resumed_agent));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let view = SessionView::replay(session, &journal)
            .expect("resumed Goal replays")
            .expect("Session exists");
        assert_eq!(view.goals[0].status, GoalStatus::Completed);
        assert_eq!(view.goals[0].run_ids, vec![first_run, resumed_run]);
        assert_eq!(view.goals[1].status, GoalStatus::Completed);
        assert_eq!(view.interrupted_goal_id(), None);
    }

    #[test]
    fn an_older_paused_goal_is_not_resumable_once_a_newer_goal_is_paused() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut paused = vec![goal_started(first, GoalBudget::default())];
        paused.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        paused.extend(cancel_run(first_run, first_agent));
        paused.push(goal_started(second, GoalBudget::default()));
        paused.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        paused.extend(cancel_run(second_run, second_agent));
        let session = SessionId::new();
        let closed_session = session_with(session, paused.clone());
        let view = SessionView::replay(session, &closed_session)
            .expect("both paused Goals replay")
            .expect("Session exists");
        assert_eq!(view.interrupted_goal_id(), Some(second));

        let mut older = paused.clone();
        older.push(Event::GoalResumed { goal_id: first });
        assert!(
            SessionView::replay_open(session, &session_with(session, older)).is_err(),
            "the older paused Goal is history, not resumable"
        );
        let mut newer = paused;
        newer.push(Event::GoalResumed { goal_id: second });
        assert!(SessionView::replay_open(session, &session_with(session, newer)).is_ok());
    }

    #[test]
    fn interrupted_goal_without_admissible_run_closes_as_failed() {
        let goal = GoalId::new();
        let (run, agent) = (RunId::new(), AgentRunId::new());
        let budget = GoalBudget {
            max_runs: 1,
            ..GoalBudget::default()
        };
        let close = Event::GoalFinished {
            goal_id: goal,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "Out of budget.".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
        };
        let mut events = vec![goal_started(goal, budget)];
        events.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(run, agent));
        let session = SessionId::new();
        let mut closing = events.clone();
        closing.push(close.clone());
        let view = SessionView::replay_open(session, &session_with(session, closing))
            .expect("an exhausted paused Goal closes as Failed")
            .expect("Session exists");
        assert_eq!(view.goal(goal).unwrap().status, GoalStatus::Failed);
        assert_eq!(view.interrupted_goal_id(), None);

        let session = SessionId::new();
        let mut admissible = vec![goal_started(goal, GoalBudget::default())];
        admissible.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        admissible.extend(cancel_run(run, agent));
        admissible.push(close);
        assert!(
            SessionView::replay_open(session, &session_with(session, admissible)).is_err(),
            "a paused Goal that still admits a Run cannot close as Failed"
        );
    }

    #[test]
    fn exhausted_goal_closes_over_a_crash_left_provider_call() {
        let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let report = GoalReport {
            summary: "Goal budget is exhausted.".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        };
        let close = Event::GoalFinished {
            goal_id: goal,
            status: GoalStatus::Failed,
            active_ms: 0,
            report,
        };
        let budget = GoalBudget {
            max_provider_calls: Some(1),
            ..GoalBudget::default()
        };
        // The process died inside the only admitted provider call: no RunFinished was journaled.
        let mut crashed = vec![goal_started(goal, budget)];
        crashed.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        crashed.push(Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: agent,
        });
        let session = SessionId::new();
        let mut closing = crashed.clone();
        closing.push(close.clone());
        let view = SessionView::replay_open(session, &session_with(session, closing))
            .expect("an exhausted crash-left Goal closes as Failed")
            .expect("Session exists");
        let goal_view = view.goal(goal).expect("Goal");
        assert_eq!(goal_view.status, GoalStatus::Failed);
        assert_eq!(goal_view.active_run, None);
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert_eq!(view.interrupted_goal_id(), None);

        let session = SessionId::new();
        let mut admissible = vec![goal_started(goal, GoalBudget::default())];
        admissible.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        admissible.push(Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: agent,
        });
        admissible.push(close);
        assert!(
            SessionView::replay_open(session, &session_with(session, admissible)).is_err(),
            "a crash-left Goal that still admits a Run cannot close as Failed"
        );
    }

    #[test]
    fn resumed_crash_left_compaction_call_settles_as_uncertain() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(run_id, agent_run_id));
        // The process died after the compaction call was reserved, before its outcome was journaled.
        events.push(Event::GoalCompactionCallReserved { goal_id, run_id });
        events.push(Event::GoalResumed { goal_id });

        let view = SessionView::replay_open(session_id, &session_with(session_id, events))
            .expect("resumed Goal replays")
            .expect("Session exists");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!(goal.status, GoalStatus::Running);
        assert_eq!(goal.compaction_call_reserved, None);
        assert_eq!(goal.uncertain_compaction_calls, 1);
        assert!(goal.token_usage_unknown);
    }

    #[test]
    fn exhausted_goal_closes_over_a_crash_left_compaction_call() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let budget = GoalBudget {
            max_provider_calls: Some(1),
            ..GoalBudget::default()
        };
        let mut events = vec![goal_started(goal_id, budget)];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(run_id, agent_run_id));
        // The only admitted call is the compaction call, whose outcome never committed.
        events.push(Event::GoalCompactionCallReserved { goal_id, run_id });
        events.push(Event::GoalFinished {
            goal_id,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "Goal budget is exhausted.".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
        });

        let session = SessionId::new();
        let view = SessionView::replay_open(session, &session_with(session, events))
            .expect("an exhausted Goal closes over its crash-left compaction call")
            .expect("Session exists");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!(goal.status, GoalStatus::Failed);
        assert_eq!(goal.compaction_call_reserved, None);
        assert_eq!(goal.uncertain_compaction_calls, 1);
    }

    #[test]
    fn exhausted_autonomous_goal_closes_over_a_crash_left_compaction_call() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let budget = GoalBudget {
            max_provider_calls: Some(2),
            ..GoalBudget::default()
        };
        let mut events = vec![Event::GoalStarted {
            goal_id,
            objective: GOAL_OBJECTIVE.into(),
            workspace_identity: (1, 2),
            budget,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Implementation,
            autonomous: true,
        }];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        events.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        events.extend([
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition: AgentDisposition::Finished,
                summary: Some("done".into()),
                result: Some("done".into()),
            },
            Event::MessageCommitted {
                run_id,
                text: "done".into(),
            },
            Event::RunFinished {
                run_id,
                disposition: RunDisposition::Finished,
            },
        ]);
        // The Run finished, but its between-Run compaction call never committed: the Goal is paused.
        events.push(Event::GoalCompactionCallReserved { goal_id, run_id });
        events.push(Event::GoalFinished {
            goal_id,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "Goal budget is exhausted.".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
        });

        let session = SessionId::new();
        let view = SessionView::replay_open(session, &session_with(session, events))
            .expect("an exhausted autonomous Goal closes over its crash-left compaction call")
            .expect("Session exists");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!(goal.status, GoalStatus::Failed);
        assert_eq!(goal.uncertain_compaction_calls, 1);
    }

    #[test]
    fn resumed_orphaned_provider_reservation_settles_as_uncertain() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let mut events = vec![
            Event::SessionStarted {
                title: "Goal".into(),
                workspace_identity: (1, 2),
            },
            goal_started(goal_id, GoalBudget::default()),
        ];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        events.push(Event::GoalResumed { goal_id });

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("resumed Goal replays")
            .expect("Session exists");
        let goal = view.goals.last().expect("Goal");
        assert!(goal.unobserved_provider_calls.is_empty());
        assert!(goal.token_usage_unknown);
    }

    #[test]
    fn finish_with_unknown_usage_is_not_a_tool_or_delegate_outcome() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let budget = GoalBudget {
            max_tokens: Some(1000),
            ..GoalBudget::default()
        };
        let mut events = vec![
            Event::SessionStarted {
                title: "Goal".into(),
                workspace_identity: (1, 2),
            },
            goal_started(goal_id, budget),
        ];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        events.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        events.push(Event::AgentFinished {
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        });

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("unknown usage does not block a Finish")
            .expect("Session exists");
        assert_eq!(view.runs[0].agents[0].status, AgentStatus::Finished);
    }

    #[test]
    fn finished_run_fails_only_an_unsatisfied_planned_goal_at_its_active_limit() {
        let limit = 1000;
        // (plan contract, its criterion satisfied, settled active time, Failed accepted)
        for (planned, satisfied, settled, accepted) in [
            (true, false, limit, true),
            (true, false, limit - 1, false),
            (false, false, limit, false),
            (true, true, limit, false),
        ] {
            let session_id = SessionId::new();
            let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
            let budget = GoalBudget {
                max_active_ms: Some(limit),
                ..GoalBudget::default()
            };
            let mut events = vec![
                Event::SessionStarted {
                    title: "Goal".into(),
                    workspace_identity: (1, 2),
                },
                Event::GoalStarted {
                    goal_id,
                    objective: GOAL_OBJECTIVE.into(),
                    workspace_identity: (1, 2),
                    budget,
                    resource_settings: crate::tools::ToolResourceSettings::default(),
                    phase: GoalPhase::Implementation,
                    autonomous: true,
                },
            ];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(CollaborationPolicy::Single, false),
            ));
            events.push(Event::GoalActiveTimeReserved {
                goal_id,
                run_id,
                reserved_ms: limit,
            });
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            });
            events.push(Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Finished,
                    None,
                ),
            });
            events.push(Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition: AgentDisposition::Finished,
                summary: Some("done".into()),
                result: Some("done".into()),
            });
            events.push(Event::MessageCommitted {
                run_id,
                text: "done".into(),
            });
            events.push(Event::GoalActiveTimeSettled {
                goal_id,
                run_id,
                active_ms: settled,
            });
            events.push(Event::RunFinished {
                run_id,
                disposition: RunDisposition::Finished,
            });
            let mut view = SessionView::replay_open(session_id, &journal(session_id, events))
                .expect("Finished Run before the Goal receipt")
                .expect("Session");
            if planned {
                let goal = view.goals.last_mut().expect("Goal");
                goal.plan_contract = Some(GoalPlanContract {
                    mission: GOAL_OBJECTIVE.into(),
                    resource_class: crate::tools::ToolResourceClass::Default,
                    command_timeout_ms: None,
                    paths: Vec::new(),
                    commands: Vec::new(),
                    skills: Vec::new(),
                    mcp_tools: Vec::new(),
                    success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                        path: "src/lib.rs".into(),
                        sha256: crate::tools::types::hex_digest(b"done"),
                    },
                    budget: GoalBudget {
                        max_active_ms: Some(limit),
                        ..GoalBudget::default()
                    },
                });
                goal.success_criterion_satisfied = satisfied;
            }
            let report = GoalReport {
                summary: "budget".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            };
            let receipt = view.finish_goal(&goal_id, &GoalStatus::Failed, &settled, &report, 0);
            assert_eq!(
                receipt.is_ok(),
                accepted,
                "a Failed receipt over a Finished Run needs an unsatisfied plan and spent active time (planned {planned}, satisfied {satisfied}, {settled} of {limit})"
            );
        }
    }

    #[test]
    fn plan_wait_does_not_block_a_terminal_failure_before_tool_start() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let mut events = vec![
            Event::SessionStarted {
                title: "Goal".into(),
                workspace_identity: (1, 2),
            },
            goal_started(goal_id, GoalBudget::default()),
        ];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, true),
        ));
        events.push(Event::GoalActiveTimeReserved {
            goal_id,
            run_id,
            reserved_ms: 1000,
        });
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        });
        events.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::ToolRequested,
                Some(0),
            ),
        });
        events.push(Event::GoalActiveTimeSettled {
            goal_id,
            run_id,
            active_ms: 0,
        });
        // Engine fails the primary when the Tool deadline or context capacity refuses before ToolStarted.
        events.push(Event::AgentFinished {
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        });
        events.push(Event::RunFinished {
            run_id,
            disposition: RunDisposition::Failed,
        });
        events.push(Event::GoalFinished {
            goal_id,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "failed".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
        });

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("a terminal failure closes the plan wait")
            .expect("Session exists");
        let goal = view.goals.last().expect("Goal");
        assert_eq!(goal.status, GoalStatus::Failed);
        assert!(!goal.awaiting_plan_contract);
        assert_eq!(view.runs[0].status, RunStatus::Failed);
    }

    #[test]
    fn unsettled_active_time_reservation_blocks_the_next_run_link() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let next_run_id = RunId::new();
        let mut events = vec![
            Event::SessionStarted {
                title: "Goal".into(),
                workspace_identity: (1, 2),
            },
            goal_started(goal_id, GoalBudget::default()),
        ];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.push(Event::GoalActiveTimeReserved {
            goal_id,
            run_id,
            reserved_ms: 1000,
        });
        events.push(Event::AgentFinished {
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        });
        events.push(Event::RunFinished {
            run_id,
            disposition: RunDisposition::Failed,
        });
        let unsettled = events.clone();
        let link_next = Event::GoalRunLinked {
            goal_id,
            run_id: next_run_id,
            phase: GoalPhase::Implementation,
        };
        let mut blocked = unsettled.clone();
        blocked.push(link_next.clone());
        assert!(
            SessionView::replay_open(session_id, &journal(session_id, blocked)).is_err(),
            "a Run cannot link while its active-time reservation is outstanding"
        );

        let mut resumed = unsettled;
        resumed.push(Event::GoalResumed { goal_id });
        resumed.push(link_next);
        SessionView::replay_open(session_id, &journal(session_id, resumed))
            .expect("explicit resume converts the orphaned reservation before linking");
    }

    fn delegated_session(known_tokens: u64) -> SessionView {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let contract = GoalPlanContract {
            mission: "Plan".into(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["src/lib.rs".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/lib.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget {
                max_tokens: Some(1000),
                ..GoalBudget::default()
            },
        };
        SessionView {
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some((1, 2)),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
                id: run_id,
                objective: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
                config: Some(config(
                    CollaborationPolicy::Auto {
                        max_active_children: 1,
                    },
                    false,
                )),
                agents: vec![AgentView {
                    id: agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: None,
                    result: None,
                    provider_calls: vec![record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::Delegated,
                        Some(0),
                    )],
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Active,
                }],
                assistant_message: None,
                status: RunStatus::Active,
                accepted_sequence: 1,
                finished_sequence: None,
                tools: Vec::new(),
                tool_compaction_summary: None,
                tool_compaction_covered_count: 0,
                display: Vec::new(),
                steering_notes: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: GOAL_OBJECTIVE.into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous: false,
                status: GoalStatus::Running,
                run_ids: vec![run_id],
                active_run: Some(run_id),
                plan_contract: Some(contract),
                success_criterion_satisfied: false,
                awaiting_plan_contract: false,
                active_ms: 0,
                uncertain_active_ms: 0,
                reserved_active_run: None,
                reserved_active_ms: None,
                compaction_call_reserved: None,
                uncertain_compaction_calls: 0,
                success_state: crate::session::GoalSuccessState::NoContract,
                resource_settings: crate::tools::ToolResourceSettings::default(),
                provider_calls: 1,
                unobserved_provider_calls: Vec::new(),
                known_tokens,
                token_usage_unknown: false,
                report: None,
                started_sequence: 1,
                finished_sequence: None,
                known_cached_tokens: 0,
            }],
        }
    }

    fn apply_child_spawn(view: &mut SessionView, grant: ChildGrant) -> Result<(), ReplayError> {
        let run_id = view.runs[0].id;
        let event = Event::AgentSpawned {
            run_id,
            agent_run_id: AgentRunId::new(),
            role: AgentRole::Child,
            ordinal: 1,
            objective: Some("Check the child result".into()),
            grant: Some(grant),
        };
        let (run_id, agent_run_id) = event.scope();
        view.apply_committed(&EventEnvelope {
            sequence: 2,
            session_id: view.id,
            run_id,
            agent_run_id,
            event,
            created_at_ms: 0,
        })
    }

    #[test]
    fn child_spawn_grant_follows_the_run_tool_policy() {
        let session_with_tools = |tools| {
            let mut view = delegated_session(0);
            view.runs[0].config = Some(config(
                CollaborationPolicy::Auto {
                    max_active_children: 1,
                },
                tools,
            ));
            view
        };
        let mut no_tools = session_with_tools(false);
        assert!(apply_child_spawn(&mut no_tools, ChildGrant::NoTools).is_ok());
        assert_eq!(no_tools.runs[0].agents[1].grant, Some(ChildGrant::NoTools));
        assert!(apply_child_spawn(&mut session_with_tools(false), ChildGrant::ReadOnly).is_err());

        let mut with_tools = session_with_tools(true);
        assert!(apply_child_spawn(&mut with_tools, ChildGrant::ReadOnly).is_ok());
        assert_eq!(
            with_tools.runs[0].agents[1].grant,
            Some(ChildGrant::ReadOnly)
        );
        assert!(apply_child_spawn(&mut session_with_tools(true), ChildGrant::NoTools).is_err());
    }

    /// Applies one Event at the next sequence, as the Store would.
    fn apply(view: &mut SessionView, event: Event) -> Result<(), ReplayError> {
        let (run_id, agent_run_id) = event.scope();
        let sequence = view.last_sequence + 1;
        view.apply_committed(&EventEnvelope {
            sequence,
            session_id: view.id,
            run_id,
            agent_run_id,
            event,
            created_at_ms: 0,
        })
    }

    fn child_intent(run_id: RunId, agent_run_id: AgentRunId, call: ToolCall) -> EffectIntent {
        EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        }
    }

    /// A delegated Tool Run with one spawned `ReadOnly` child; returns (goal, run, child) ids.
    fn research_session() -> (SessionView, GoalId, RunId, AgentRunId) {
        let mut view = delegated_session(0);
        view.runs[0].config = Some(config(
            CollaborationPolicy::Auto {
                max_active_children: 1,
            },
            true,
        ));
        apply_child_spawn(&mut view, ChildGrant::ReadOnly).expect("research child spawn");
        let (goal, run, child) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[1].id);
        (view, goal, run, child)
    }

    fn reserve_child_call(goal_id: GoalId, run_id: RunId, agent_run_id: AgentRunId) -> Event {
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        }
    }

    fn child_record(
        run_id: RunId,
        agent_run_id: AgentRunId,
        disposition: ProviderCallDisposition,
    ) -> Event {
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(AgentPhase::ChildWork, disposition, Some(0)),
        }
    }

    /// Whether `event` is rejected when applied after the accepted prefix in `view`.
    fn rejected(view: &SessionView, event: Event) -> bool {
        apply(&mut view.clone(), event).is_err()
    }

    #[test]
    fn child_research_tools_replay_in_order_and_reject_widening() {
        let (mut view, goal, run, child) = research_session();
        let read = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: 0,
                limit: 1024,
            },
        );
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        assert!(rejected(
            &view,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: read.clone(),
            }
        ));
        let mut no_tools = delegated_session(0);
        apply_child_spawn(&mut no_tools, ChildGrant::NoTools).expect("no-tools child spawn");
        let (no_tools_goal, no_tools_run, no_tools_child) = (
            no_tools.goals[0].id,
            no_tools.runs[0].id,
            no_tools.runs[0].agents[1].id,
        );
        apply(
            &mut no_tools,
            reserve_child_call(no_tools_goal, no_tools_run, no_tools_child),
        )
        .expect("no-tools child call");
        assert!(rejected(
            &no_tools,
            child_record(
                no_tools_run,
                no_tools_child,
                ProviderCallDisposition::ToolRequested
            ),
        ));
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("child Tool request");
        let started = Event::ToolStarted {
            run_id: run,
            agent_run_id: child,
            intent: read.clone(),
        };
        let finished = Event::ToolFinished {
            run_id: run,
            agent_run_id: child,
            observation: observation_for(read.clone(), b"ok"),
        };
        let after_request = view.clone();
        let write = child_intent(
            run,
            child,
            ToolCall::Write {
                path: "src/lib.rs".into(),
                expected_digest: None,
                content: "x".into(),
            },
        );
        assert!(rejected(
            &after_request,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: write,
            }
        ));
        let mcp = child_intent(
            run,
            child,
            ToolCall::McpCall {
                server: "local".into(),
                tool: "probe".into(),
                schema_digest: "a".repeat(64),
                arguments: "{}".into(),
            },
        );
        assert!(rejected(
            &after_request,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: mcp,
            }
        ));
        for call in [
            ToolCall::GitStatus { path: ".".into() },
            ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 1,
            },
        ] {
            let intent = child_intent(run, child, call);
            assert!(rejected(
                &after_request,
                Event::ToolStarted {
                    run_id: run,
                    agent_run_id: child,
                    intent,
                }
            ));
        }
        apply(&mut view, started.clone()).expect("child Tool starts");
        let mut open = view.clone();
        apply(&mut open, reserve_child_call(goal, run, child)).expect("second reservation");
        assert!(rejected(
            &open,
            child_record(run, child, ProviderCallDisposition::Finished)
        ));
        assert!(rejected(
            &view,
            Event::AgentFinished {
                run_id: run,
                agent_run_id: child,
                disposition: AgentDisposition::Cancelled,
                summary: None,
                result: None,
            }
        ));
        let mismatched = ToolObservation {
            intent: child_intent(
                run,
                child,
                ToolCall::Read {
                    path: "src/lib.rs".into(),
                    offset: 0,
                    limit: 2,
                },
            ),
            ..observation_for(read.clone(), b"ok")
        };
        assert!(rejected(
            &view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: mismatched,
            }
        ));
        apply(&mut view, finished).expect("child Tool finishes");

        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::Finished),
        )
        .expect("child finishes");
        apply(
            &mut view,
            Event::AgentFinished {
                run_id: run,
                agent_run_id: child,
                disposition: AgentDisposition::Finished,
                summary: Some("report".into()),
                result: Some("report".into()),
            },
        )
        .expect("child report");
        assert!(view.runs[0].tools.is_empty());
        assert_eq!(view.runs[0].agents[1].tools.len(), 1);
        assert!(!view.goals[0].awaiting_plan_contract);
        assert!(!view.goals[0].success_criterion_satisfied);
    }

    #[test]
    fn child_tool_outside_the_plan_replays_only_as_denied() {
        let (mut view, goal, run, child) = research_session();
        let outside = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 1024,
            },
        );
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("child Tool request");
        apply(
            &mut view,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: outside.clone(),
            },
        )
        .expect("child Tool starts");
        assert!(rejected(
            &view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: observation_for(outside.clone(), b"x"),
            }
        ));
        apply(
            &mut view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: ToolObservation {
                    intent: outside,
                    disposition: ToolDisposition::Denied,
                    output:
                        "The effect is outside the active Goal plan; no operation was dispatched"
                            .into(),
                    guard: None,
                    cache_update: CacheUpdate::NotApplicable,
                },
            },
        )
        .expect("out-of-plan child read is denied");
    }

    #[test]
    fn child_research_tool_bound_admits_sixteen_requests_and_no_seventeenth() {
        let (mut view, goal, run, child) = research_session();
        for _ in 0..crate::session::MAX_CHILD_TOOL_CALLS {
            let read = child_intent(
                run,
                child,
                ToolCall::Read {
                    path: "src/lib.rs".into(),
                    offset: 0,
                    limit: 1024,
                },
            );
            apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
            apply(
                &mut view,
                child_record(run, child, ProviderCallDisposition::ToolRequested),
            )
            .expect("child Tool request");
            apply(
                &mut view,
                Event::ToolStarted {
                    run_id: run,
                    agent_run_id: child,
                    intent: read.clone(),
                },
            )
            .expect("child Tool starts");
            apply(
                &mut view,
                Event::ToolFinished {
                    run_id: run,
                    agent_run_id: child,
                    observation: observation_for(read, b"ok"),
                },
            )
            .expect("child Tool finishes");
        }
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        assert!(rejected(
            &view,
            child_record(run, child, ProviderCallDisposition::ToolRequested)
        ));
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::Finished),
        )
        .expect("child may still finish at the bound");
    }

    #[test]
    fn crash_left_child_tool_interrupts_the_run_and_blocks_goal_resume() {
        let (mut view, goal, run, child) = research_session();
        let read = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: 0,
                limit: 1024,
            },
        );
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("child Tool request");
        apply(
            &mut view,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: read.clone(),
            },
        )
        .expect("child Tool starts");
        let mut crashed = view.clone();
        apply(
            &mut crashed,
            goal_started(GoalId::new(), GoalBudget::default()),
        )
        .expect("a new Goal seals the crashed Goal");
        assert_eq!(crashed.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
        assert_eq!(crashed.runs[0].agents[1].status, AgentStatus::Interrupted);
        assert!(!crashed.goals[0].can_resume(&crashed));

        apply(
            &mut view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: observation_for(read, b"ok"),
            },
        )
        .expect("child Tool finishes");
        let mut settled = view.clone();
        apply(
            &mut settled,
            goal_started(GoalId::new(), GoalBudget::default()),
        )
        .expect("a new Goal seals the settled Goal");
        assert!(settled.goals[0].can_resume(&settled));
    }

    #[test]
    fn child_admission_uses_the_effective_plan_token_ceiling() {
        let mut below_cap = delegated_session(999);
        assert!(apply_child_spawn(&mut below_cap, ChildGrant::NoTools).is_ok());

        let mut at_cap = delegated_session(1000);
        assert!(apply_child_spawn(&mut at_cap, ChildGrant::NoTools).is_err());
    }

    fn read_observation(path: &str, content: &[u8]) -> ToolObservation {
        file_observation(
            ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1024,
            },
            content,
        )
    }

    fn file_observation(call: ToolCall, content: &[u8]) -> ToolObservation {
        let intent = EffectIntent {
            id: uuid::Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        observation_for(intent, content)
    }

    fn observation_for(intent: EffectIntent, content: &[u8]) -> ToolObservation {
        ToolObservation {
            guard: Some(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
            intent,
            disposition: ToolDisposition::Succeeded,
            output: serde_json::json!({
                "sha256": crate::tools::types::hex_digest(content)
            })
            .to_string(),
            cache_update: CacheUpdate::NotApplicable,
        }
    }

    #[test]
    fn plan_contract_counts_a_pre_plan_read_matching_its_digest() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let read = read_observation("src/main.rs", b"before");
        let read_intent = read.intent.clone();
        let contract = GoalPlanContract {
            mission: "Verify the file".into(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["src/main.rs".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                path: "src/main.rs".into(),
                sha256: crate::tools::types::hex_digest(b"before"),
            },
            budget: GoalBudget::default(),
        };
        let mut view = SessionView {
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some((1, 2)),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
                id: run_id,
                objective: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
                config: Some(config(CollaborationPolicy::Single, true)),
                agents: vec![AgentView {
                    id: agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: None,
                    result: None,
                    provider_calls: vec![
                        record(
                            AgentPhase::RootPlan,
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        ),
                        record(
                            AgentPhase::RootPlan,
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        ),
                    ],
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Active,
                }],
                assistant_message: None,
                status: RunStatus::Active,
                accepted_sequence: 1,
                finished_sequence: None,
                tools: vec![ToolView {
                    intent: read_intent,
                    observation: Some(read),
                    review_call_index: None,
                    finished_sequence: Some(2),
                }],
                tool_compaction_summary: None,
                tool_compaction_covered_count: 0,
                display: Vec::new(),
                steering_notes: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: GOAL_OBJECTIVE.into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                resource_settings: crate::tools::ToolResourceSettings::default(),
                phase: GoalPhase::Research,
                autonomous: false,
                status: GoalStatus::Running,
                run_ids: vec![run_id],
                active_run: Some(run_id),
                plan_contract: None,
                success_criterion_satisfied: false,
                awaiting_plan_contract: true,
                active_ms: 0,
                uncertain_active_ms: 0,
                reserved_active_run: None,
                reserved_active_ms: None,
                compaction_call_reserved: None,
                uncertain_compaction_calls: 0,
                success_state: crate::session::GoalSuccessState::NoContract,
                provider_calls: 2,
                unobserved_provider_calls: Vec::new(),
                known_tokens: 0,
                known_cached_tokens: 0,
                token_usage_unknown: false,
                report: None,
                started_sequence: 1,
                finished_sequence: None,
            }],
        };
        view.apply_committed(&EventEnvelope {
            sequence: 2,
            session_id,
            run_id: Some(run_id),
            agent_run_id: None,
            event: Event::GoalPlanContractRecorded {
                goal_id,
                run_id,
                contract,
            },
            created_at_ms: 0,
        })
        .expect("contract records");
        assert!(view.goals[0].success_criterion_satisfied);
    }

    #[test]
    fn plan_contract_counts_a_write_from_an_earlier_run() {
        let session_id = SessionId::new();
        let (goal_id, earlier_run_id, run_id, agent_run_id) =
            (GoalId::new(), RunId::new(), RunId::new(), AgentRunId::new());
        let write = file_observation(
            ToolCall::Write {
                path: "src/main.rs".into(),
                expected_digest: None,
                content: "after".into(),
            },
            b"after",
        );
        let contract = GoalPlanContract {
            mission: "Write the file".into(),
            paths: vec!["src/main.rs".into()],
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "after".into(),
            },
            budget: GoalBudget::default(),
        };
        let mut view = SessionView {
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some((1, 2)),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 2,
            lineage: None,
            runs: vec![
                RunView {
                    id: earlier_run_id,
                    objective: GOAL_OBJECTIVE.into(),
                    images: Vec::new(),
                    config: Some(config(CollaborationPolicy::Single, true)),
                    agents: Vec::new(),
                    assistant_message: None,
                    status: RunStatus::Finished,
                    accepted_sequence: 1,
                    finished_sequence: Some(1),
                    tools: vec![ToolView {
                        intent: write.intent.clone(),
                        observation: Some(write),
                        review_call_index: None,
                        finished_sequence: Some(1),
                    }],
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    display: Vec::new(),
                    steering_notes: Vec::new(),
                },
                RunView {
                    id: run_id,
                    objective: GOAL_OBJECTIVE.into(),
                    images: Vec::new(),
                    config: Some(config(CollaborationPolicy::Single, true)),
                    agents: vec![AgentView {
                        id: agent_run_id,
                        role: AgentRole::Primary,
                        ordinal: 0,
                        objective: None,
                        summary: None,
                        result: None,
                        provider_calls: vec![record(
                            AgentPhase::RootPlan,
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        )],
                        grant: None,
                        tools: Vec::new(),
                        status: AgentStatus::Active,
                    }],
                    assistant_message: None,
                    status: RunStatus::Active,
                    accepted_sequence: 2,
                    finished_sequence: None,
                    tools: Vec::new(),
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    display: Vec::new(),
                    steering_notes: Vec::new(),
                },
            ],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: GOAL_OBJECTIVE.into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                resource_settings: crate::tools::ToolResourceSettings::default(),
                phase: GoalPhase::Research,
                autonomous: false,
                status: GoalStatus::Running,
                run_ids: vec![earlier_run_id, run_id],
                active_run: Some(run_id),
                plan_contract: None,
                success_state: crate::session::GoalSuccessState::NoContract,
                success_criterion_satisfied: false,
                awaiting_plan_contract: true,
                active_ms: 0,
                uncertain_active_ms: 0,
                reserved_active_run: None,
                reserved_active_ms: None,
                provider_calls: 2,
                unobserved_provider_calls: Vec::new(),
                compaction_call_reserved: None,
                uncertain_compaction_calls: 0,
                known_tokens: 0,
                known_cached_tokens: 0,
                token_usage_unknown: false,
                report: None,
                started_sequence: 1,
                finished_sequence: None,
            }],
        };
        view.apply_committed(&EventEnvelope {
            sequence: 3,
            session_id,
            run_id: Some(run_id),
            agent_run_id: None,
            event: Event::GoalPlanContractRecorded {
                goal_id,
                run_id,
                contract,
            },
            created_at_ms: 0,
        })
        .expect("contract records");
        assert!(view.goals[0].success_criterion_satisfied);
        assert_eq!(
            view.goals[0].success_state,
            crate::session::GoalSuccessState::WorkspaceFile {
                sha256: Some(crate::tools::types::hex_digest(b"after")),
            }
        );
    }

    /// A tool-enabled primary AgentRun with no provider calls, under a Goal with or without its plan.
    fn primary_tool_session(planned: bool) -> (SessionView, GoalId, RunId, AgentRunId) {
        let mut view = delegated_session(0);
        view.runs[0].config = Some(config(CollaborationPolicy::Single, true));
        view.runs[0].agents[0].provider_calls.clear();
        if !planned {
            view.goals[0].plan_contract = None;
        }
        let (goal, run, agent) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[0].id);
        (view, goal, run, agent)
    }

    fn read_intent(run: RunId, agent: AgentRunId, path: &str) -> EffectIntent {
        child_intent(
            run,
            agent,
            ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1024,
            },
        )
    }

    fn tool_started(run: RunId, agent: AgentRunId, intent: &EffectIntent) -> Event {
        Event::ToolStarted {
            run_id: run,
            agent_run_id: agent,
            intent: intent.clone(),
        }
    }

    fn tool_finished(
        run: RunId,
        agent: AgentRunId,
        intent: &EffectIntent,
        content: &[u8],
    ) -> Event {
        Event::ToolFinished {
            run_id: run,
            agent_run_id: agent,
            observation: observation_for(intent.clone(), content),
        }
    }

    fn requesting_step(run: RunId, agent: AgentRunId, requested_tools: u8) -> Event {
        Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: ProviderCallRecord {
                requested_tools,
                ..record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::ToolRequested,
                    Some(0),
                )
            },
        }
    }

    #[test]
    fn tool_started_requires_pending_request() {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        let (first, second, third) = (
            read_intent(run, agent, "src/lib.rs"),
            read_intent(run, agent, "src/main.rs"),
            read_intent(run, agent, "README.md"),
        );
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
        assert!(
            rejected(&view, tool_started(run, agent, &first)),
            "no request is declared before the step"
        );
        apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
        apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
        apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
        apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
        apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
        assert!(
            rejected(&view, tool_started(run, agent, &third)),
            "every declared request is already started"
        );
    }

    #[test]
    fn continuation_requires_all_requested_observed() {
        let (mut view, goal, run, agent) = primary_tool_session(true);
        let (first, second) = (
            read_intent(run, agent, "src/lib.rs"),
            read_intent(run, agent, "src/lib.rs"),
        );
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
        apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
        apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
        apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("continuation call");
        let continuation = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        };
        assert!(
            rejected(&view, continuation.clone()),
            "the second declared request is unobserved"
        );
        apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
        apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
        apply(&mut view, continuation).expect("all declared requests observed");
    }

    /// A planned Goal whose completed two-request step has a compaction call reserved.
    fn compacting_session() -> (SessionView, GoalId, RunId) {
        let (mut view, goal, run, agent) = primary_tool_session(true);
        let (first, second) = (
            read_intent(run, agent, "src/lib.rs"),
            read_intent(run, agent, "src/lib.rs"),
        );
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
        apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
        apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
        apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
        apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
        apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
        apply(
            &mut view,
            Event::GoalCompactionCallReserved {
                goal_id: goal,
                run_id: run,
            },
        )
        .expect("compaction call reserved");
        (view, goal, run)
    }

    /// A Failed compaction record covering the first `count` Tools, ending at the last of them.
    fn covering_compaction(view: &SessionView, goal: GoalId, run: RunId, count: u32) -> Event {
        let covered = &view.runs[0].tools[count as usize - 1];
        Event::GoalContextCompacted {
            goal_id: goal,
            run_id: run,
            covered_tool_count: count,
            record: crate::session::events::CompactionRecord {
                covered_run_id: run,
                covered_sequence: covered.finished_sequence.expect("observed Tool"),
                source_digest: [0; 32],
                provider: "openai".into(),
                model: "gpt-5.4".into(),
                prompt_version: 1,
                schema_version: 1,
                compiler_version: 3,
                source_bytes: 1,
                source_tokens_estimate: 1,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                response_id: None,
                wire_provenance: None,
                chatgpt_provenance: None,
                output_token_bound: OutputTokenBound::ProviderEnforced,
                status: crate::session::events::CompactionStatus::Failed {
                    reason: crate::session::events::CompactionFailure::ProviderUnavailable,
                },
            },
        }
    }

    #[test]
    fn compaction_prefix_never_splits_a_step() {
        let (view, goal, run) = compacting_session();
        assert!(
            rejected(&view, covering_compaction(&view, goal, run, 1)),
            "a covered prefix inside the two-request step"
        );
        let whole = covering_compaction(&view, goal, run, 2);
        let mut replayed = view.clone();
        apply(&mut replayed, whole).expect("the whole step is covered");
        assert_eq!(replayed.goals[0].compaction_call_reserved, None);
    }

    #[test]
    fn goal_report_cached_tokens_must_match_the_goal_total() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let cached_call = ProviderCallRecord {
            cache_read_input_tokens: Some(5),
            ..record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(5),
            )
        };
        let finish = |cached_input_tokens| {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(CollaborationPolicy::Single, false),
            ));
            events.extend([
                Event::GoalProviderCallReserved {
                    goal_id,
                    run_id,
                    agent_run_id,
                },
                Event::ProviderCallRecorded {
                    run_id,
                    agent_run_id,
                    record: cached_call.clone(),
                },
                Event::AgentFinished {
                    run_id,
                    agent_run_id,
                    disposition: AgentDisposition::Finished,
                    summary: Some("done".into()),
                    result: Some("done".into()),
                },
                Event::MessageCommitted {
                    run_id,
                    text: "done".into(),
                },
                Event::RunFinished {
                    run_id,
                    disposition: RunDisposition::Finished,
                },
                Event::GoalFinished {
                    goal_id,
                    status: GoalStatus::Completed,
                    active_ms: 0,
                    report: GoalReport {
                        summary: "done".into(),
                        changes: Vec::new(),
                        evidence: Vec::new(),
                        unresolved: Vec::new(),
                        cached_input_tokens,
                    },
                },
            ]);
            SessionView::replay(session_id, &session_with(session_id, events))
        };
        let view = finish(5).expect("matching cache total").expect("Session");
        assert_eq!(
            view.goals[0].report.as_ref().map(|r| r.cached_input_tokens),
            Some(5)
        );
        assert!(
            finish(0).is_err(),
            "a report may not understate cache reads"
        );
        assert!(finish(6).is_err(), "a report may not overstate cache reads");
    }

    #[test]
    fn child_tool_request_count_is_one() {
        let (mut view, goal, run, child) = research_session();
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        let widened = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: child,
            record: ProviderCallRecord {
                requested_tools: 2,
                ..record(
                    AgentPhase::ChildWork,
                    ProviderCallDisposition::ToolRequested,
                    Some(0),
                )
            },
        };
        assert!(rejected(&view, widened), "a child step declares one Tool");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("single child request");
    }
}
