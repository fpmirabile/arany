use super::events::{
    AgentDisposition, AgentRole, AgentRunId, CompactionRecord, Event, EventEnvelope, ForkLineage,
    GoalBudget, GoalId, GoalPhase, GoalPlanContract, GoalReport, GoalStatus,
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
pub struct GoalView {
    pub id: GoalId,
    pub objective: String,
    pub workspace_identity: (u64, u64),
    pub budget: GoalBudget,
    pub phase: Option<GoalPhase>,
    pub status: GoalStatus,
    pub run_ids: Vec<RunId>,
    pub active_run: Option<RunId>,
    pub plan_contract: Option<GoalPlanContract>,
    pub success_criterion_satisfied: bool,
    pub awaiting_plan_contract: bool,
    pub active_ms: u64,
    pub uncertain_active_ms: u64,
    pub reserved_active_run: Option<RunId>,
    pub reserved_active_ms: Option<u64>,
    pub provider_calls: u32,
    pub unobserved_provider_calls: Vec<AgentRunId>,
    pub compaction_call_reserved: Option<RunId>,
    pub uncertain_compaction_calls: u32,
    pub known_tokens: u64,
    pub token_usage_unknown: bool,
    pub report: Option<GoalReport>,
    pub started_sequence: u64,
    pub finished_sequence: Option<u64>,
}

impl GoalView {
    pub fn is_active(&self) -> bool {
        matches!(self.status, GoalStatus::Running | GoalStatus::Interrupted)
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
                    run.tools.iter().all(|tool| {
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
                display: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: "Inspect before planning".into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                phase: Some(GoalPhase::Research),
                status: GoalStatus::Interrupted,
                run_ids: vec![run_id],
                active_run: None,
                plan_contract: None,
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
    pub display: Vec<super::events::DisplayChunk>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolView {
    pub intent: crate::tools::EffectIntent,
    pub observation: Option<crate::tools::ToolObservation>,
    pub review_call_index: Option<u8>,
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
            !matches!(
                tool.intent.call,
                crate::tools::ToolCall::ApplyChangeset { .. }
            )
        })
        .count()
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
    }) || model_tool_count(run)
        != primary
            .provider_calls
            .iter()
            .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            .count()
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
        && model_tool_count(run)
            == agent
                .provider_calls
                .iter()
                .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                .count()
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

    pub(crate) fn tool_context(&self) -> String {
        if self.tools.is_empty() {
            return String::new();
        }
        let records: Vec<_> = self.tools.iter().map(|tool| serde_json::json!({
            "intent":tool.intent,
            "disposition":tool.observation.as_ref().map(|observation| observation.disposition),
            "cache_update":tool.observation.as_ref().map(|observation| observation.cache_update),
            "output":tool.observation.as_ref().map(|observation| &observation.output),
            "execution":if tool.observation.is_none() { "uncertain; never replay" } else { "observed; never replay" },
        })).collect();
        serde_json::json!({"scope":"historical Run; no pending Tool in the current Run; never replay intents or approvals; fresh inspection may establish present state", "run_status":format!("{:?}", self.status),"tool_records":records})
            .to_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionView {
    pub event_sequence: u64,
    pub record: CompactionRecord,
}

impl RunView {
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
            if let Some(goal) = current.goals.last_mut()
                && goal.status == GoalStatus::Running
            {
                goal.status = GoalStatus::Interrupted;
                goal.active_run = None;
            }
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
        if current
            .goals
            .last()
            .is_some_and(|goal| goal.awaiting_plan_contract)
            && !matches!(
                &envelope.event,
                Event::GoalPlanContractRecorded { goal_id, .. }
                    if current.goals.last().is_some_and(|goal| goal.id == *goal_id)
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
                Event::GoalResumed { goal_id }
                    if current.goals.last().is_some_and(|goal| goal.id == *goal_id)
            )
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
            Event::SessionRenamed { title } => {
                current.title = title.clone();
                current.title_is_explicit = true;
            }
            Event::SessionDefaultChanged { defaults } => {
                current.defaults = defaults.clone();
            }
            Event::GoalStarted {
                goal_id,
                objective,
                workspace_identity,
                budget,
            } => {
                if current.goals.iter().any(|goal| goal.id == *goal_id)
                    || current.goals.last().is_some_and(GoalView::is_active)
                    || current
                        .workspace_identity
                        .is_some_and(|identity| identity != *workspace_identity)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                current.workspace_identity = Some(*workspace_identity);
                current.goals.push(GoalView {
                    id: *goal_id,
                    objective: objective.clone(),
                    workspace_identity: *workspace_identity,
                    budget: budget.clone(),
                    phase: None,
                    status: GoalStatus::Running,
                    run_ids: Vec::new(),
                    active_run: None,
                    plan_contract: None,
                    success_criterion_satisfied: false,
                    awaiting_plan_contract: false,
                    active_ms: 0,
                    uncertain_active_ms: 0,
                    reserved_active_run: None,
                    reserved_active_ms: None,
                    provider_calls: 0,
                    unobserved_provider_calls: Vec::new(),
                    compaction_call_reserved: None,
                    uncertain_compaction_calls: 0,
                    known_tokens: 0,
                    token_usage_unknown: false,
                    report: None,
                    started_sequence: prior,
                    finished_sequence: None,
                });
            }
            Event::GoalRunLinked {
                goal_id,
                run_id,
                phase,
            } => {
                if current.runs.iter().any(|run| run.id == *run_id) {
                    return Err(ReplayError::InvalidTransition);
                }
                let goal = current_goal_mut(current, *goal_id)?;
                let run_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_runs, |plan| plan.budget.max_runs);
                if goal.status != GoalStatus::Running
                    || goal.active_run.is_some()
                    || goal.run_ids.len() >= usize::from(run_limit)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_run = Some(*run_id);
                goal.phase = Some(*phase);
                goal.run_ids.push(*run_id);
            }
            Event::GoalResumed { goal_id } => {
                let active_run = current
                    .goals
                    .last()
                    .filter(|goal| goal.id == *goal_id && goal.status == GoalStatus::Running)
                    .and_then(|goal| goal.active_run);
                if let Some(run_id) = active_run
                    && let Some(run) = current.runs.last_mut().filter(|run| run.id == run_id)
                {
                    run.interrupt();
                }
                if let Some(goal) = current
                    .goals
                    .last_mut()
                    .filter(|goal| goal.id == *goal_id && goal.status == GoalStatus::Running)
                {
                    goal.status = GoalStatus::Interrupted;
                    goal.active_run = None;
                }
                let recoverable = current
                    .goals
                    .last()
                    .filter(|goal| goal.id == *goal_id)
                    .is_some_and(|goal| goal.can_resume(current));
                let has_unaccepted_run = current.goals.last().is_some_and(|goal| {
                    goal.run_ids
                        .last()
                        .is_some_and(|run_id| !current.runs.iter().any(|run| run.id == *run_id))
                });
                let goal = current_goal_mut(current, *goal_id)?;
                if goal.status != GoalStatus::Interrupted
                    || goal.active_run.is_some()
                    || !recoverable
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if has_unaccepted_run {
                    goal.run_ids.pop();
                }
                if goal.status == GoalStatus::Interrupted {
                    let reservation = match (goal.reserved_active_run, goal.reserved_active_ms) {
                        (Some(run_id), Some(reserved_ms))
                            if goal.run_ids.last() == Some(&run_id) =>
                        {
                            Some(reserved_ms)
                        }
                        (None, None) => None,
                        _ => return Err(ReplayError::InvalidTransition),
                    };
                    if let Some(reserved_ms) = reservation {
                        goal.active_ms = goal
                            .active_ms
                            .checked_add(reserved_ms)
                            .ok_or(ReplayError::InvalidTransition)?;
                        goal.uncertain_active_ms = goal
                            .uncertain_active_ms
                            .checked_add(reserved_ms)
                            .ok_or(ReplayError::InvalidTransition)?;
                        goal.reserved_active_run = None;
                        goal.reserved_active_ms = None;
                    }
                    if let Some(run_id) = goal.compaction_call_reserved.take() {
                        if goal.run_ids.last() != Some(&run_id) {
                            return Err(ReplayError::InvalidTransition);
                        }
                        goal.uncertain_compaction_calls = goal
                            .uncertain_compaction_calls
                            .checked_add(1)
                            .ok_or(ReplayError::InvalidTransition)?;
                        goal.token_usage_unknown = true;
                    }
                } else if goal.reserved_active_run.is_some() || goal.reserved_active_ms.is_some() {
                    return Err(ReplayError::InvalidTransition);
                }
                let active_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_active_ms, |plan| plan.budget.max_active_ms);
                if goal.active_ms > active_limit {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.awaiting_plan_contract = false;
                goal.status = GoalStatus::Running;
            }
            Event::GoalActiveTimeReserved {
                goal_id,
                run_id,
                reserved_ms,
            } => {
                let goal = current_goal_mut(current, *goal_id)?;
                let consumed = goal
                    .active_ms
                    .checked_add(*reserved_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                let active_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_active_ms, |plan| plan.budget.max_active_ms);
                if goal.status != GoalStatus::Running
                    || goal.active_run != Some(*run_id)
                    || goal.reserved_active_run.is_some()
                    || goal.reserved_active_ms.is_some()
                    || consumed > active_limit
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.reserved_active_run = Some(*run_id);
                goal.reserved_active_ms = Some(*reserved_ms);
            }
            Event::GoalActiveTimeSettled {
                goal_id,
                run_id,
                active_ms,
            } => {
                let run_status = current_run(current, *run_id)?.status;
                let goal = current_goal_mut(current, *goal_id)?;
                let reserved = goal
                    .reserved_active_ms
                    .ok_or(ReplayError::InvalidTransition)?;
                let total = goal
                    .active_ms
                    .checked_add(*active_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                let active_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_active_ms, |plan| plan.budget.max_active_ms);
                if goal.status != GoalStatus::Running
                    || goal.active_run.is_some()
                    || goal.reserved_active_run != Some(*run_id)
                    || *active_ms > reserved
                    || !run_status.is_terminal()
                    || total > active_limit
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_ms += *active_ms;
                goal.reserved_active_run = None;
                goal.reserved_active_ms = None;
            }
            Event::GoalPlanContractRecorded {
                goal_id,
                run_id,
                contract,
                active_ms,
                reserved_ms,
            } => {
                let run = current
                    .runs
                    .last()
                    .filter(|run| run.id == *run_id)
                    .ok_or(ReplayError::InvalidTransition)?;
                let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
                let valid_previous_tools = run.tools.iter().all(|tool| {
                    tool.intent.call.is_goal_plan_prelude()
                        && tool
                            .observation
                            .as_ref()
                            .is_some_and(|observation| !observation.stops_run())
                });
                let requested_tools = primary
                    .provider_calls
                    .iter()
                    .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                    .count();
                let valid_contract_call = run.status == RunStatus::Active
                    && primary.role == AgentRole::Primary
                    && primary.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                            && matches!(
                                call.phase,
                                AgentPhase::RootPlan | AgentPhase::RootSynthesis
                            )
                    })
                    && valid_previous_tools
                    && requested_tools == model_tool_count(run) + 1;
                let goal = current_goal_mut(current, *goal_id)?;
                let reserved = goal
                    .reserved_active_ms
                    .ok_or(ReplayError::InvalidTransition)?;
                let active_total = goal
                    .active_ms
                    .checked_add(*active_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                let reserved_total = active_total
                    .checked_add(*reserved_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                if goal.status != GoalStatus::Running
                    || goal.active_run != Some(*run_id)
                    || goal.reserved_active_run != Some(*run_id)
                    || goal.plan_contract.is_some()
                    || !goal.awaiting_plan_contract
                    || !valid_contract_call
                    || !contract.budget.is_within(&goal.budget)
                    || goal.provider_calls > contract.budget.max_provider_calls
                    || goal.run_ids.len() > usize::from(contract.budget.max_runs)
                    || *active_ms > reserved
                    || active_ms.saturating_add(*reserved_ms) > reserved
                    || reserved_total > contract.budget.max_active_ms
                    || contract
                        .budget
                        .max_tokens
                        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_ms = active_total;
                goal.reserved_active_ms = Some(*reserved_ms);
                goal.plan_contract = Some(contract.clone());
                goal.success_criterion_satisfied = false;
                goal.awaiting_plan_contract = false;
            }
            Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
            } => {
                let agent_active = current_run(current, *run_id)?
                    .agents
                    .iter()
                    .any(|agent| agent.id == *agent_run_id && agent.status == AgentStatus::Active);
                let goal = current_goal_mut(current, *goal_id)?;
                let goal_call_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_provider_calls, |plan| {
                        plan.budget.max_provider_calls
                    });
                let goal_token_limit = goal
                    .plan_contract
                    .as_ref()
                    .and_then(|plan| plan.budget.max_tokens)
                    .or(goal.budget.max_tokens);
                if goal.status != GoalStatus::Running
                    || goal.active_run != Some(*run_id)
                    || !agent_active
                    || goal.unobserved_provider_calls.contains(agent_run_id)
                    || goal.provider_calls >= goal_call_limit
                    || goal.compaction_call_reserved.is_some()
                    || goal_token_limit.is_some_and(|limit| {
                        goal.token_usage_unknown
                            || goal.known_tokens >= limit
                            || !goal.unobserved_provider_calls.is_empty()
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.provider_calls = goal
                    .provider_calls
                    .checked_add(1)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.unobserved_provider_calls.push(*agent_run_id);
            }
            Event::GoalCompactionCallReserved { goal_id, run_id } => {
                let run_status = current_run(current, *run_id)?.status;
                let goal = current_goal_mut(current, *goal_id)?;
                let call_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_provider_calls, |plan| {
                        plan.budget.max_provider_calls
                    });
                let token_limit = goal
                    .plan_contract
                    .as_ref()
                    .and_then(|plan| plan.budget.max_tokens)
                    .or(goal.budget.max_tokens);
                if goal.status != GoalStatus::Running
                    || goal.active_run.is_some()
                    || goal.run_ids.last() != Some(run_id)
                    || !run_status.is_terminal()
                    || !goal.unobserved_provider_calls.is_empty()
                    || goal.compaction_call_reserved.is_some()
                    || goal.provider_calls >= call_limit
                    || token_limit
                        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.provider_calls = goal
                    .provider_calls
                    .checked_add(1)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.compaction_call_reserved = Some(*run_id);
            }
            Event::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
            } => {
                let run_id = current
                    .goals
                    .last()
                    .filter(|goal| goal.id == *goal_id)
                    .and_then(|goal| goal.run_ids.last())
                    .copied()
                    .ok_or(ReplayError::InvalidTransition)?;
                let run_status = current_run(current, run_id)?.status;
                let disposition_matches = matches!(
                    (*status, run_status),
                    (GoalStatus::Completed, RunStatus::Finished)
                        | (GoalStatus::Failed, RunStatus::Failed)
                        | (GoalStatus::Cancelled, RunStatus::Cancelled)
                );
                let goal = current_goal_mut(current, *goal_id)?;
                let active_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_active_ms, |plan| plan.budget.max_active_ms);
                if goal.status != GoalStatus::Running
                    || goal.active_run.is_some()
                    || !disposition_matches
                    || goal.reserved_active_run.is_some()
                    || goal.reserved_active_ms.is_some()
                    || goal.compaction_call_reserved.is_some()
                    || *active_ms != goal.active_ms
                    || *active_ms > active_limit
                    || *status == GoalStatus::Completed
                        && goal.plan_contract.is_some()
                        && !goal.success_criterion_satisfied
                {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_ms = *active_ms;
                goal.status = *status;
                goal.report = Some(report.clone());
                goal.finished_sequence = Some(prior);
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
                if let Some(goal) = current.goals.last_mut()
                    && goal.status == GoalStatus::Running
                    && goal.run_ids.last() == Some(&record.covered_run_id)
                {
                    if goal.compaction_call_reserved != Some(record.covered_run_id) {
                        return Err(ReplayError::InvalidTransition);
                    }
                    let tokens = u64::from(record.input_tokens.unwrap_or_default())
                        .checked_add(u64::from(record.output_tokens.unwrap_or_default()))
                        .and_then(|tokens| goal.known_tokens.checked_add(tokens))
                        .ok_or(ReplayError::InvalidTransition)?;
                    goal.known_tokens = tokens;
                    goal.token_usage_unknown |=
                        record.input_tokens.is_none() || record.output_tokens.is_none();
                    goal.compaction_call_reserved = None;
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
                let goal = current.goals.last().ok_or(ReplayError::InvalidTransition)?;
                if goal.status != GoalStatus::Running
                    || goal.active_run != Some(*run_id)
                    || goal.run_ids.first() == Some(run_id) && goal.objective != *text
                {
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
                    display: Vec::new(),
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
                let goal = current.goals.last().ok_or(ReplayError::InvalidTransition)?;
                if goal.status != GoalStatus::Running
                    || goal.active_run != Some(*run_id)
                    || goal.workspace_identity != workspace_identity
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
                if *role == AgentRole::Child {
                    let goal = current.goals.last().ok_or(ReplayError::InvalidTransition)?;
                    if goal.active_run != Some(*run_id)
                        || goal.provider_calls >= goal.budget.max_provider_calls
                        || goal.budget.max_tokens.is_some()
                            && (goal.token_usage_unknown
                                || goal.known_tokens >= goal.budget.max_tokens.unwrap_or(0))
                    {
                        return Err(ReplayError::InvalidTransition);
                    }
                }
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
                        .is_none_or(|call| call.disposition != ProviderCallDisposition::Delegated)
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
                let goal = current.goals.last().ok_or(ReplayError::InvalidTransition)?;
                if goal.active_run != Some(*run_id)
                    || !goal.unobserved_provider_calls.contains(agent_run_id)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                let goal_id = goal.id;
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
                let tool_continuation = tools_enabled && is_tool_continuation(run, agent, record);
                let delegated = tools_enabled && is_delegated_synthesis(run, agent, record);
                let tool_review = tools_enabled && is_tool_review(run, agent, record);
                let valid = tool_continuation
                    || tool_review
                    || delegated
                    || match (agent.role, agent.provider_calls.as_slice(), record.phase) {
                        (AgentRole::Primary, [], AgentPhase::RootPlan) => true,
                        (AgentRole::Primary, [first], AgentPhase::RootSynthesis)
                            if first.disposition == ProviderCallDisposition::Delegated =>
                        {
                            all_children_finished(run)
                        }
                        (AgentRole::Child, [], AgentPhase::ChildWork) => true,
                        _ => false,
                    };
                if agent.status != AgentStatus::Active
                    || !valid
                    || record.disposition == ProviderCallDisposition::ToolRequested
                        && (!tools_enabled
                            || agent.role != AgentRole::Primary
                            || record.phase == crate::provider::AgentPhase::ToolReview)
                    || tools_enabled && agent.provider_calls.len() >= crate::tools::MAX_MODEL_STEPS
                    || (record.phase != AgentPhase::RootPlan
                        && record.disposition == ProviderCallDisposition::Delegated)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if run.agents[index].role == AgentRole::Primary
                    && matches!(
                        record.disposition,
                        ProviderCallDisposition::Delegated | ProviderCallDisposition::ToolRequested
                    )
                    && run.display.iter().any(|chunk| {
                        chunk.category == super::events::DisplayCategory::Answer
                            && usize::from(chunk.call_index)
                                == run.agents[index].provider_calls.len()
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if record.phase == AgentPhase::ToolReview {
                    run.tools
                        .last_mut()
                        .ok_or(ReplayError::InvalidTransition)?
                        .review_call_index = Some(run.agents[index].provider_calls.len() as u8);
                }
                run.agents[index].provider_calls.push(record.clone());
                let goal = current_goal_mut(current, goal_id)?;
                let reservation_index = goal
                    .unobserved_provider_calls
                    .iter()
                    .position(|reserved| reserved == agent_run_id)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.unobserved_provider_calls.remove(reservation_index);
                goal.known_tokens = goal
                    .known_tokens
                    .checked_add(u64::from(record.input_tokens.unwrap_or_default()))
                    .and_then(|tokens| {
                        tokens.checked_add(u64::from(record.output_tokens.unwrap_or_default()))
                    })
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.token_usage_unknown |=
                    record.input_tokens.is_none() || record.output_tokens.is_none();
                if record.disposition == ProviderCallDisposition::ToolRequested
                    && goal.plan_contract.is_none()
                {
                    if goal.awaiting_plan_contract {
                        return Err(ReplayError::InvalidTransition);
                    }
                    goal.awaiting_plan_contract = true;
                }
            }
            Event::AssistantDisplayRecorded {
                run_id,
                agent_run_id,
                chunk,
            } => {
                use super::events::{
                    DisplayCategory, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
                    MAX_DISPLAY_TEXT_BYTES,
                };
                let run = active_run(current, *run_id)?;
                let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
                let candidate = ProviderCallRecord {
                    phase: chunk.phase,
                    disposition: ProviderCallDisposition::Finished,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    wire_provenance: None,
                    failure_reason: None,
                };
                let valid_phase = primary.provider_calls.is_empty()
                    && chunk.phase == AgentPhase::RootPlan
                    || is_delegated_synthesis(run, primary, &candidate)
                    || is_tool_continuation(run, primary, &candidate);
                if primary.id != *agent_run_id
                    || primary.status != AgentStatus::Active
                    || chunk.call_index as usize != primary.provider_calls.len()
                    || !valid_phase
                    || run.display.len() >= MAX_DISPLAY_CHUNKS
                {
                    return Err(ReplayError::InvalidTransition);
                }
                let offset: usize = run
                    .display
                    .iter()
                    .filter(|prior| {
                        prior.call_index == chunk.call_index
                            && prior.category == chunk.category
                            && prior.item == chunk.item
                    })
                    .map(|prior| prior.text.len())
                    .sum();
                let total: usize = run
                    .display
                    .iter()
                    .filter(|prior| prior.category == chunk.category)
                    .map(|prior| prior.text.len())
                    .sum();
                let mut items = std::collections::HashSet::new();
                for prior in &run.display {
                    if prior.category == DisplayCategory::Commentary {
                        items.insert((prior.call_index, prior.item));
                    }
                }
                if chunk.category == DisplayCategory::Commentary {
                    items.insert((chunk.call_index, chunk.item));
                }
                if chunk.category == DisplayCategory::Limit
                    && run
                        .display
                        .iter()
                        .any(|prior| prior.category == DisplayCategory::Limit)
                    || chunk.offset as usize != offset
                    || total + chunk.text.len() > MAX_DISPLAY_TEXT_BYTES
                    || items.len() > MAX_DISPLAY_COMMENTARY_ITEMS
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.display.push(chunk.clone());
            }
            Event::ToolStarted {
                run_id,
                agent_run_id,
                intent,
            } => {
                let goal = current.goals.last().ok_or(ReplayError::InvalidTransition)?;
                let phase_allows_call = goal.phase == Some(GoalPhase::Implementation)
                    || intent.call.is_goal_plan_prelude();
                let plan_allows_call = if let Some(contract) = &goal.plan_contract {
                    !goal.awaiting_plan_contract
                        && (contract.allows_call(&intent.call)
                            || matches!(
                                &intent.call,
                                crate::tools::ToolCall::ApplyChangeset { .. }
                            ))
                } else {
                    goal.awaiting_plan_contract && intent.call.is_goal_plan_prelude()
                };
                if goal.active_run != Some(*run_id)
                    || !phase_allows_call
                    || !plan_allows_call
                    || goal
                        .plan_contract
                        .as_ref()
                        .and_then(|plan| plan.budget.max_tokens)
                        .or(goal.budget.max_tokens)
                        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                let run = active_run(current, *run_id)?;
                let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
                let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
                let integration =
                    matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. });
                let valid_origin = if integration {
                    valid_integration_source(run, intent)
                } else {
                    primary.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                    }) && primary
                        .provider_calls
                        .iter()
                        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                        .count()
                        == model_tool_count(run) + 1
                };
                if config
                    .tool_policy
                    .as_ref()
                    .is_none_or(|policy| policy.config_digest != intent.policy_digest)
                    || primary.id != *agent_run_id
                    || primary.status != AgentStatus::Active
                    || !valid_origin
                    || run.tools.len() >= crate::tools::MAX_TOOL_CALLS
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
                    review_call_index: None,
                });
            }
            Event::ToolFinished {
                run_id,
                agent_run_id,
                observation,
            } => {
                let goal = current
                    .goals
                    .last()
                    .filter(|goal| goal.active_run == Some(*run_id))
                    .ok_or(ReplayError::InvalidTransition)?;
                let out_of_plan_integration = matches!(
                    &observation.intent.call,
                    crate::tools::ToolCall::ApplyChangeset { .. }
                ) && goal
                    .plan_contract
                    .as_ref()
                    .is_some_and(|contract| !contract.allows_call(&observation.intent.call));
                if out_of_plan_integration
                    && (observation.disposition != crate::tools::ToolDisposition::Denied
                        || observation.guard.is_some()
                        || observation.cache_update != crate::tools::CacheUpdate::NotApplicable)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                {
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
                        .filter_map(|tool| tool.observation.as_ref().cloned())
                        .collect();
                    if !serde_json::to_vec(&observations)
                        .is_ok_and(|bytes| bytes.len() <= crate::tools::MAX_TOOL_CONTEXT_BYTES)
                    {
                        return Err(ReplayError::InvalidTransition);
                    }
                }
                let goal_id = current
                    .goals
                    .last()
                    .filter(|goal| goal.active_run == Some(*run_id))
                    .map(|goal| goal.id)
                    .ok_or(ReplayError::InvalidTransition)?;
                let observations = goal_tool_observations(current, goal_id)?;
                if let Some(goal) = current.goals.last_mut()
                    && let Some(contract) = &goal.plan_contract
                {
                    goal.success_criterion_satisfied = contract.matches_success(&observations);
                }
                let unplanned_prelude = current
                    .goals
                    .last()
                    .is_some_and(|goal| goal.plan_contract.is_none())
                    && observation.intent.call.is_goal_plan_prelude();
                if unplanned_prelude && let Some(goal) = current.goals.last_mut() {
                    goal.awaiting_plan_contract = false;
                }
            }
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition,
                summary,
                result,
            } => {
                let token_budget_exceeded = current.goals.last().is_none_or(|goal| {
                    goal.budget
                        .max_tokens
                        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens > limit)
                        || goal.plan_contract.as_ref().is_some_and(|plan| {
                            plan.budget.max_tokens.is_some_and(|limit| {
                                goal.token_usage_unknown || goal.known_tokens > limit
                            })
                        })
                });
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
                            || tool
                                .observation
                                .as_ref()
                                .is_some_and(|observation| observation.stops_run())
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if *disposition == AgentDisposition::Finished
                    && run.agents[index].provider_calls.last().is_none_or(|call| {
                        call.disposition != ProviderCallDisposition::Finished
                            || call.phase == crate::provider::AgentPhase::ToolReview
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if *disposition == AgentDisposition::Finished && token_budget_exceeded {
                    return Err(ReplayError::InvalidTransition);
                }
                if *disposition == AgentDisposition::Finished
                    && run.agents[index].role == AgentRole::Primary
                {
                    let call = run.agents[index].provider_calls.len().saturating_sub(1);
                    for chunk in run.display.iter().filter(|chunk| {
                        chunk.category == super::events::DisplayCategory::Answer
                            && usize::from(chunk.call_index) == call
                    }) {
                        let start = chunk.offset as usize;
                        if result.as_ref().is_none_or(|text| {
                            text.get(start..start + chunk.text.len()) != Some(chunk.text.as_str())
                        }) {
                            return Err(ReplayError::InvalidTransition);
                        }
                    }
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
                if let Some(goal) = current.goals.last_mut()
                    && goal.active_run == Some(*run_id)
                {
                    goal.active_run = None;
                }
            }
        }
        current.last_sequence = prior;
        Ok(())
    }

    pub fn active_goal(&self) -> Option<&GoalView> {
        self.goals.last().filter(|goal| goal.is_active())
    }

    pub fn goal(&self, id: GoalId) -> Option<&GoalView> {
        self.goals.iter().find(|goal| goal.id == id)
    }
}

fn current_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    view.runs
        .last_mut()
        .filter(|run| run.id == id)
        .ok_or(ReplayError::InvalidTransition)
}

fn current_goal_mut(view: &mut SessionView, id: GoalId) -> Result<&mut GoalView, ReplayError> {
    view.goals
        .last_mut()
        .filter(|goal| goal.id == id)
        .ok_or(ReplayError::InvalidTransition)
}

fn goal_tool_observations(
    view: &SessionView,
    id: GoalId,
) -> Result<Vec<crate::tools::ToolObservation>, ReplayError> {
    let goal = view
        .goals
        .last()
        .filter(|goal| goal.id == id)
        .ok_or(ReplayError::InvalidTransition)?;
    Ok(view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .flat_map(|run| run.tools.iter())
        .filter_map(|tool| tool.observation.clone())
        .collect())
}

fn active_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    let run = current_run(view, id)?;
    if run.status != RunStatus::Active {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(run)
}
