use super::events::{
    AgentDisposition, AgentRole, AgentRunId, CompactionRecord, Event, EventEnvelope, ForkLineage,
    GoalBudget, GoalId, GoalPhase, GoalPlanContract, GoalReport, GoalStatus, GoalUsage,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition, RunId,
    SessionDefaults, SessionId, WorkspaceIdentity,
};
use crate::provider::AgentPhase;

mod agent;
mod worker;
pub use worker::{WorkerState, WorkerView};
pub(crate) use worker::{
    coordination_ready, criterion_unmet_in_view, evidence_admitted, private_criterion_evidence,
    private_failure, verification_passed, wave_settled,
};
mod goal;
mod rewind;
#[cfg(test)]
pub(crate) mod rewind_tests;
mod run;
mod session;
mod tool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    StoppedByHook,
    Pending,
    Active,
    Finished,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    fn is_terminal(self) -> bool {
        self == Self::Interrupted || self.is_outcome()
    }

    pub fn is_outcome(self) -> bool {
        matches!(
            self,
            Self::Finished | Self::Failed | Self::Cancelled | Self::StoppedByHook
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    StoppedByHook,
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
    pub budget_refusal: Option<super::BudgetRefusal>,
    pub(crate) run_start_usage: super::GoalUsage,
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
        if self.active_run.is_some() || super::rewind::goal_blocked(view, self) {
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
                    run.hooks_settled()
                        && run
                            .tools
                            .iter()
                            .all(|t| t.settled_without_stop() || matches!(&t.intent.call, crate::tools::ToolCall::PrepareWorker { preparation } if preparation.purpose == super::worker::WorkerDataPurpose::Base))
                        && run.agents.iter().all(|agent| {
                            agent.tools.iter().all(|t| {
                                t.settled_without_stop()
                                    || agent.assignment.as_ref().is_some_and(|w| {
                                        matches!(agent.grant, Some(super::ChildGrant::EditOnly { .. }))
                                            && matches!(t.intent.target, super::worker::EffectTarget::WorkerView { assignment_id, base_digest, .. } if assignment_id == agent.id && base_digest == w.pins.base.sha256)
                                            && t.hooks.iter().all(|h| h.outcome.as_ref().is_some_and(|o| !o.observation.stops_run()))
                                    })
                            })
                        })
                })
        })
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod worker_tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentView {
    pub profile: Option<String>,
    pub assignment: Option<WorkerView>,
    pub definition: Option<String>,
    pub id: AgentRunId,
    pub role: AgentRole,
    pub ordinal: u8,
    pub objective: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub provider_calls: Vec<ProviderCallRecord>,
    pub grant: Option<crate::session::ChildGrant>,
    pub steering_notes: Vec<String>,
    /// Read-only research Tools of a child; the primary's Tools stay in `RunView::tools`.
    pub tools: Vec<ToolView>,
    pub status: AgentStatus,
}

const MAX_PRIMARY_REFUSALS: usize = 8;

impl AgentView {
    pub(crate) fn refusal_limit_reached(&self) -> bool {
        let Some(refusal) = self
            .provider_calls
            .last()
            .and_then(|call| call.refusal.as_ref())
        else {
            return false;
        };
        self.provider_calls
            .iter()
            .filter(|call| call.refusal.is_some())
            .count()
            >= MAX_PRIMARY_REFUSALS
            || self
                .provider_calls
                .iter()
                .rev()
                .take(3)
                .filter(|call| call.refusal.as_ref() == Some(refusal))
                .count()
                == 3
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunView {
    pub skill: Option<String>,
    pub supervisor: crate::supervisor::SupervisorState,
    pub workers: WorkerState,
    pub id: RunId,
    pub objective: String,
    pub images: Vec<crate::provider::ImageAttachment>,
    pub config: Option<RunConfig>,
    pub provider_fallbacks: Vec<(AgentRunId, crate::provider::ProviderFallback)>,
    pub provider_retries: Vec<(AgentRunId, crate::provider::ProviderRetry)>,
    pub agents: Vec<AgentView>,
    pub assistant_message: Option<String>,
    pub status: RunStatus,
    pub accepted_sequence: u64,
    pub finished_sequence: Option<u64>,
    pub failure: Option<super::RunFailure>,
    pub tools: Vec<ToolView>,
    pub tool_compaction_summary: Option<String>,
    pub tool_compaction_covered_count: u32,
    pub tool_results_cleared_count: u32,
    pub definitions: DefinitionCache,
    pub display: Vec<super::events::DisplayChunk>,
    pub steering_notes: Vec<String>,
    pub lifecycle_hooks: Vec<HookView>,
    pub lifecycle_hook_sequence: Option<u64>,
    pub provider_sequences: std::collections::HashMap<AgentRunId, u64>,
}

impl RunView {
    // Parse steps always lead the Run's tools and never reach the model.
    pub(crate) fn parse_count(&self) -> usize {
        self.tools
            .iter()
            .take_while(|tool| {
                matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::ParseAgentDefinition { .. }
                )
            })
            .count()
    }

    pub(crate) fn is_invoked_skill_call(&self, call: &crate::tools::ToolCall) -> bool {
        matches!(call, crate::tools::ToolCall::Skill { name, resource: None } if self.skill.as_ref() == Some(name))
    }

    pub(crate) fn invoked_skill_read(&self) -> Option<&ToolView> {
        self.tools
            .get(self.parse_count())
            .filter(|tool| self.is_invoked_skill_call(&tool.intent.call))
    }

    pub(crate) fn initial_tool_count(&self) -> usize {
        self.parse_count() + usize::from(self.invoked_skill_read().is_some())
    }

    pub(crate) fn live_tool_records(&self) -> impl Iterator<Item = (usize, &ToolView)> {
        self.tools
            .iter()
            .enumerate()
            .skip((self.tool_compaction_covered_count as usize).max(self.initial_tool_count()))
            .filter(|(_, tool)| {
                !matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::PrepareWorker { .. }
                )
            })
    }

    // No Tool of the Run has an open or run-stopping hook; a Command's hooks sit on its source Tool.
    pub(crate) fn hooks_settled(&self) -> bool {
        self.lifecycle_hooks.iter().all(|hook| {
            hook.outcome
                .as_ref()
                .is_some_and(|outcome| !outcome.observation.stops_run())
        }) && self
            .tools
            .iter()
            .chain(self.agents.iter().flat_map(|agent| &agent.tools))
            .all(|tool| {
                tool.hooks.iter().all(|hook| {
                    hook.outcome
                        .as_ref()
                        .is_some_and(|outcome| !outcome.observation.stops_run())
                })
            })
    }

    fn stopped_hooks(&self) -> impl Iterator<Item = (&HookView, Option<&str>)> {
        self.lifecycle_hooks
            .iter()
            .chain(
                self.tools
                    .iter()
                    .chain(self.agents.iter().flat_map(|agent| &agent.tools))
                    .flat_map(|tool| &tool.hooks),
            )
            .filter_map(|hook| match &hook.outcome.as_ref()?.result {
                crate::tools::permissions::HookResult::Stopped { reason } => {
                    Some((hook, reason.as_deref()))
                }
                _ => None,
            })
    }

    pub fn hook_stop_reason(&self) -> Option<Option<&str>> {
        self.stopped_hooks().next().map(|(_, reason)| reason)
    }

    pub(crate) fn actor_stopped_by_hook(&self, agent: AgentRunId) -> bool {
        self.stopped_hooks()
            .any(|(hook, _)| hook.invocation.intent.agent_run_id == agent)
    }

    pub(crate) fn stop_hooks(&self, agent: AgentRunId) -> impl Iterator<Item = &HookView> {
        let index = self
            .agents
            .iter()
            .find(|view| view.id == agent)
            .map_or(0, |view| view.provider_calls.len());
        let subject = crate::tools::permissions::stop_subject(agent, index.saturating_sub(1));
        self.lifecycle_hooks.iter().filter(move |hook| {
            hook.invocation.intent.agent_run_id == agent
                && hook.invocation.subject_digest == subject
                && matches!(
                    hook.invocation.event,
                    crate::tools::permissions::HookEvent::Stop
                        | crate::tools::permissions::HookEvent::SubagentStop
                )
        })
    }

    pub(crate) fn stop_blocked(&self, agent: AgentRunId) -> bool {
        self.stop_hooks(agent).any(|hook| {
            if self.primary().is_some_and(|primary| primary.id == agent)
                && self.hook_rerun_proposal(hook).is_some()
            {
                return true;
            }
            hook.outcome.as_ref().is_some_and(|outcome| {
                matches!(
                    outcome.result,
                    crate::tools::permissions::HookResult::Blocked { .. }
                )
            })
        })
    }

    pub(crate) fn stop_continuations(&self, agent: AgentRunId) -> usize {
        let Some(view) = self.agents.iter().find(|view| view.id == agent) else {
            return 0;
        };
        view.provider_calls
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let subject = crate::tools::permissions::stop_subject(agent, *index);
                self.lifecycle_hooks.iter().any(|hook| {
                    hook.invocation.intent.agent_run_id == agent
                        && hook.invocation.subject_digest == subject
                        && hook.outcome.as_ref().is_some_and(|outcome| {
                            matches!(
                                outcome.result,
                                crate::tools::permissions::HookResult::Blocked { .. }
                            ) || self.primary().is_some_and(|primary| primary.id == agent)
                                && outcome.observation.enforcement_cause.is_some()
                        })
                })
            })
            .count()
    }

    pub(crate) fn model_tools(&self) -> &[ToolView] {
        &self.tools[self.initial_tool_count()..]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolView {
    pub checkpoint: Option<super::Checkpoint>,
    pub intent: crate::tools::EffectIntent,
    pub observation: Option<crate::tools::ToolObservation>,
    pub review_call_index: Option<u32>,
    pub permission_verdict: Option<crate::tools::permissions::PermissionVerdict>,
    pub hooks: Vec<HookView>,
    pub hook_sequence: Option<u64>,
    pub finished_sequence: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookView {
    pub(crate) sequence: u64,
    pub invocation: crate::tools::permissions::HookInvocation,
    pub outcome: Option<crate::tools::permissions::HookOutcome>,
    pub(crate) idle: bool,
}

impl ToolView {
    // Model-facing PostToolUse lines, derived from the journaled hook results alone.
    pub fn hook_feedback(&self) -> Vec<String> {
        crate::tools::permissions::feedback_lines(
            self.hooks
                .iter()
                .filter(|hook| {
                    hook.invocation.event == crate::tools::permissions::HookEvent::PostToolUse
                })
                .filter_map(|hook| hook.outcome.as_ref())
                .map(|outcome| &outcome.result),
        )
    }

    // A hook whose outcome is unknown stops the Run exactly like an uncertain Tool.
    pub(crate) fn settled_without_stop(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|observation| !observation.stops_run())
            && !self.hooks.iter().any(|hook| {
                hook.outcome
                    .as_ref()
                    .is_none_or(|outcome| outcome.observation.stops_run())
            })
    }

    pub fn ran_host(&self) -> bool {
        self.intent.call.is_host()
            && match &self.observation {
                Some(observation) => {
                    matches!(
                        observation.attestation,
                        Some(crate::tools::Attestation::Host(_))
                    ) || observation.disposition == crate::tools::ToolDisposition::Uncertain
                }
                None => self
                    .permission_verdict
                    .as_ref()
                    .is_some_and(|verdict| verdict.approved),
            }
    }

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
    run.agents.len() > 1 && run.agents.iter().skip(1).all(worker::child_terminal)
}

impl ToolView {
    // The journaled acceptance: the verdict says the Engine narrowed this Tool and the hook result names the call.
    pub(crate) fn narrowed_call(&self) -> Option<&crate::tools::ToolCall> {
        if !self.permission_verdict.as_ref()?.narrowed {
            return None;
        }
        match self.hooks.last()?.outcome.as_ref()?.result {
            crate::tools::permissions::HookResult::Narrowed { ref call } => Some(call),
            _ => None,
        }
    }
}

fn model_tool_count(run: &RunView) -> usize {
    run.tools
        .iter()
        .skip(run.initial_tool_count())
        .filter(|tool| {
            tool.narrowed_call().is_none()
                && !is_integration_tool(tool)
                && !matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::ParseAgentDefinition { .. }
                        | crate::tools::ToolCall::PrepareWorker { .. }
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
        && run.hooks_settled()
        && run.tools.last().is_some_and(|tool| {
            tool.settled_without_stop()
                && tool.observation.as_ref().is_some_and(|observation| {
                    observed_command_changeset(observation)
                        .is_none_or(|changeset| changeset.files.is_empty())
                })
        })
        && requested_tool_total(agent).checked_sub(model_tool_count(run)) == Some(0)
}

fn is_delegated_synthesis(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Primary
        && agent.provider_calls.last().is_some_and(|call| {
            call.disposition == ProviderCallDisposition::Delegated
                || call.phase == AgentPhase::RootSynthesis
                    && call.disposition == ProviderCallDisposition::InvalidResponse
                    && agent.provider_calls.len() > 1
                    && run.workers.stop.is_none()
                    && run.agents.iter().any(|a| a.assignment.is_some())
        })
        && record.phase == AgentPhase::RootSynthesis
        && (all_children_finished(run)
            || record.disposition != ProviderCallDisposition::Delegated
                && worker::coordination_ready(run))
}

impl RunView {
    fn hook_rerun_proposal(&self, hook: &HookView) -> Option<String> {
        let crate::tools::ToolCall::Command { program, args, cwd } = &hook.invocation.intent.call
        else {
            return None;
        };
        let call = crate::tools::ToolCall::CommandUnguarded {
            program: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            justification: "Re-run the hook command after a Guard enforcement failure".into(),
        };
        if self
            .unguarded_failure(&call)
            .is_none_or(|(observation, is_hook)| {
                !is_hook || observation.intent.id != hook.invocation.intent.id
            })
            || self.tools.iter().any(|tool| {
                tool.observation.is_some()
                    && crate::tools::types::guarded_command_matches(
                        &tool.intent.call,
                        &hook.invocation.intent.call,
                    )
            })
        {
            return None;
        }
        let proposal = format!(
            "Hook ignored after a Guard enforcement failure. A human may approve this exact re-run outside the Guard: {}",
            serde_json::to_string(&call).expect("typed command")
        );
        (proposal.len() <= crate::tools::permissions::MAX_HOOK_FEEDBACK_BYTES).then_some(proposal)
    }

    pub(crate) fn hook_rerun_proposals(&self, subject: Option<[u8; 32]>) -> Vec<String> {
        let mut bytes = 0;
        self.tools
            .iter()
            .chain(self.agents.iter().flat_map(|agent| agent.tools.iter()))
            .flat_map(|tool| tool.hooks.iter())
            .chain(self.lifecycle_hooks.iter())
            .filter(|hook| subject.is_none_or(|subject| hook.invocation.subject_digest == subject))
            .filter_map(|hook| self.hook_rerun_proposal(hook))
            .filter(|proposal| {
                bytes += proposal.len();
                bytes <= crate::tools::permissions::MAX_HOOK_FEEDBACK_BYTES
            })
            .collect()
    }

    pub(crate) fn unguarded_failure(
        &self,
        call: &crate::tools::ToolCall,
    ) -> Option<(&crate::tools::ToolObservation, bool)> {
        let primary = self.primary()?;
        let policy = self.config.as_ref()?.tool_policy.as_ref()?;
        if policy.unguarded != crate::tools::UnguardedGrant::HumanOnly {
            return None;
        }
        let allowed = |agent| {
            agent == primary.id
                || self.agents.iter().any(|child| {
                    child.id == agent
                        && child.grant.is_some_and(|grant| {
                            grant.unguarded() == crate::tools::UnguardedGrant::HumanOnly
                                && grant.unguarded().narrows(policy.child_unguarded)
                        })
                })
        };
        let hook = self
            .tools
            .iter()
            .chain(self.agents.iter().flat_map(|agent| agent.tools.iter()))
            .filter(|tool| tool.observation.is_some())
            .flat_map(|tool| tool.hooks.iter())
            .chain(self.lifecycle_hooks.iter())
            .filter(|hook| allowed(hook.invocation.intent.agent_run_id))
            .filter(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    crate::tools::types::guarded_command_matches(
                        call,
                        &outcome.observation.intent.call,
                    )
                })
            })
            .max_by_key(|hook| hook.sequence);
        crate::tools::types::qualifying_hook_failure(
            call,
            hook.into_iter()
                .filter_map(|hook| hook.outcome.as_ref().map(|outcome| &outcome.observation)),
        )
        .map(|observation| (observation, true))
        .or_else(|| {
            crate::tools::types::qualifying_guard_failure(
                call,
                self.tools
                    .iter()
                    .filter_map(|tool| tool.observation.as_ref())
                    .filter(|observation| observation.intent.agent_run_id == primary.id),
            )
            .map(|observation| (observation, false))
        })
    }

    pub fn host_tools(&self) -> usize {
        self.tools.iter().filter(|tool| tool.ran_host()).count()
    }

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
    pub(crate) workspace_identity: Option<WorkspaceIdentity>,
    pub(crate) rewinds: super::rewind::RewindState,
    pub(crate) runs: Vec<RunView>,
    pub(crate) compactions: Vec<CompactionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionView {
    pub event_sequence: u64,
    pub record: CompactionRecord,
}

impl RunView {
    pub(crate) fn steering_bytes(&self) -> usize {
        self.steering_notes
            .iter()
            .map(|note| note.len() + super::MAX_STEERING_LABEL_BYTES)
            .sum::<usize>()
            + self
                .agents
                .iter()
                .flat_map(|agent| &agent.steering_notes)
                .map(|note| note.len() + super::MAX_CHILD_STEERING_LABEL_BYTES)
                .sum::<usize>()
    }

    pub(crate) fn history_user_text(&self) -> String {
        let mut text = self.objective.clone();
        if !self.steering_notes.is_empty() {
            text.push_str("\n\nSteering notes the user sent during this Run:");
            for (index, note) in self.steering_notes.iter().enumerate() {
                use std::fmt::Write;
                write!(&mut text, "\n[{}] {note}", index + 1).expect("String formatting");
            }
        }
        if self
            .agents
            .iter()
            .any(|agent| !agent.steering_notes.is_empty())
        {
            use std::fmt::Write;
            text.push_str("\n\nSteering notes the user sent to children during this Run:");
            for agent in &self.agents {
                for (index, note) in agent.steering_notes.iter().enumerate() {
                    write!(
                        &mut text,
                        "\n[child {} / {}] {note}",
                        agent.ordinal,
                        index + 1
                    )
                    .expect("String formatting");
                }
            }
        }
        text
    }

    pub(crate) fn provider_chain(
        &self,
        actor: AgentRunId,
        role: crate::provider::ProviderRole,
    ) -> Option<&[crate::provider::ProviderIdentity]> {
        let config = self.config.as_ref()?;
        if role == crate::provider::ProviderRole::Child
            && let Some(roster) = &config.roster
        {
            let agent = self.agents.iter().find(|agent| agent.id == actor)?;
            return roster.chain(agent.profile.as_deref()?);
        }
        config
            .providers
            .as_ref()
            .map(|routes| routes.get(role).as_slice())
    }

    pub(crate) fn primary(&self) -> Option<&AgentView> {
        self.agents
            .first()
            .filter(|agent| agent.role == AgentRole::Primary)
    }

    pub(crate) fn compaction_matches(&self, record: &CompactionRecord) -> bool {
        let Some(config) = &self.config else {
            return true;
        };
        let initial = config
            .providers
            .as_ref()
            .and_then(|routes| routes.compaction.first())
            .or_else(|| {
                let roster = config.roster.as_ref()?;
                roster.chain(&roster.providers.compaction)?.first()
            });
        let Some(initial) = initial else {
            return config.roster.is_none();
        };
        let identity = self
            .provider_fallbacks
            .iter()
            .rev()
            .find(|(_, fallback)| fallback.role == crate::provider::ProviderRole::Compaction)
            .map(|(_, fallback)| &fallback.to)
            .unwrap_or(initial);
        identity.limits.output_token_bound.admits_reported_output(
            record.output_tokens,
            identity
                .limits
                .max_output_tokens
                .min(super::MAX_COMPACTION_OUTPUT_TOKENS),
        ) && identity.profile == record.provider
            && identity.model == record.model
            && identity.chatgpt_provenance() == record.chatgpt_provenance.as_ref()
            && identity.limits.output_token_bound == record.output_token_bound
    }

    fn interrupt(&mut self) {
        if !self.status.is_terminal() {
            self.status = RunStatus::Interrupted;
            self.workers.clear_pending();
            for agent in &mut self.agents {
                if let Some(w) = &mut agent.assignment {
                    w.interrupted = true;
                }
                if agent.status == AgentStatus::Active {
                    agent.status = AgentStatus::Interrupted;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionView {
    pub rewinds: super::rewind::RewindState,
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
            .next()
            .map(|run| super::title_preview(&run.objective))
            .or_else(|| self.inherited_title.clone())
            .unwrap_or_else(|| "Empty conversation".into())
    }

    pub fn replay(id: SessionId, events: &[EventEnvelope]) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, true, None, None)
    }

    pub(crate) fn replay_open(
        id: SessionId,
        events: &[EventEnvelope],
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, false, None, None)
    }

    pub(crate) fn replay_forked(
        id: SessionId,
        events: &[EventEnvelope],
        close_history: bool,
        inherited: &ForkInheritance,
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, close_history, Some(inherited), None)
    }

    pub(crate) fn replay_for_corpus(
        id: SessionId,
        events: &[EventEnvelope],
        inherited: Option<&ForkInheritance>,
        corpus: &mut crate::supervisor::corpus::Corpus,
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, true, inherited, Some(corpus))
    }

    fn replay_prefix(
        id: SessionId,
        events: &[EventEnvelope],
        close_history: bool,
        inherited: Option<&ForkInheritance>,
        mut corpus: Option<&mut crate::supervisor::corpus::Corpus>,
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
                        rewinds: Default::default(),
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
                    view.as_mut()
                        .expect("created Session")
                        .rewinds
                        .prefix
                        .update(envelope)?;
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
                        workspace_identity: inherited.and_then(|i| i.workspace_identity),
                        defaults: SessionDefaults::default(),
                        created_sequence: prior,
                        last_sequence: prior,
                        lineage: Some(ForkLineage {
                            source_session_id: *source_session_id,
                            source_run_id: *source_run_id,
                            source_sequence: *source_sequence,
                            prefix_digest: *prefix_digest,
                        }),
                        rewinds: inherited.map_or_else(Default::default, |i| {
                            let mut r = i.rewinds.clone();
                            r.prefix = Default::default();
                            r
                        }),
                        runs: inherited.map_or_else(Vec::new, |inherited| inherited.runs.clone()),
                        compactions: inherited
                            .map_or_else(Vec::new, |inherited| inherited.compactions.clone()),
                        goals: Vec::new(),
                    });
                    view.as_mut()
                        .expect("created Session")
                        .rewinds
                        .prefix
                        .update(envelope)?;
                    continue;
                }
                _ => {}
            }
            let current = view.as_mut().ok_or(ReplayError::InvalidTransition)?;
            current.apply_committed(envelope)?;
            if let Some(corpus) = &mut corpus {
                corpus.observe(current, envelope);
            }
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
        if close_history && let Some(current) = &mut view {
            rewind::seal(current);
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
        if crate::supervisor::SupervisorState::before(self, envelope)? {
            self.rewinds.prefix.update(envelope)?;
            self.last_sequence = envelope.sequence;
            return Ok(());
        }
        let prior = envelope.sequence;
        let current = self;
        rewind::before_event(current, &envelope.event);
        if current
            .runs
            .last()
            .is_some_and(|run| run.status == RunStatus::Active && run.hook_stop_reason().is_some())
            && matches!(
                envelope.event,
                Event::ToolStarted { .. }
                    | Event::HookStarted { .. }
                    | Event::GoalProviderCallReserved { .. }
                    | Event::GoalCompactionCallReserved { .. }
                    | Event::AgentSpawned { .. }
                    | Event::ProviderFallbackUsed { .. }
                    | Event::ProviderRetryScheduled { .. }
            )
        {
            return Err(ReplayError::InvalidTransition);
        }
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
                Event::GoalResumed { goal_id } if *goal_id == awaiting
            )
            && !matches!(
                &envelope.event,
                Event::GoalStarted { .. }
                    | Event::WorkspaceRewindStarted { .. }
                    | Event::WorkspaceRewindFinished { .. }
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
        tool::before_lifecycle_event(current, &envelope.event)?;
        worker::before_event(current, &envelope.event)?;
        match &envelope.event {
            Event::SupervisorVerdictRecorded { .. } => return Err(ReplayError::InvalidTransition),
            Event::WorkspaceRewindStarted { record } => rewind::start(current, record, prior)?,
            Event::WorkspaceRewindFinished { record } => rewind::finish(current, record, prior)?,
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
            | Event::ToolResultsCleared { .. }
            | Event::GoalFinished { .. }
            | Event::GoalSteeringNoteRecorded { .. }
            | Event::AgentSteeringNoteRecorded { .. } => {
                goal::apply(current, &envelope.event, prior)?
            }
            Event::MessageAccepted { .. }
            | Event::RunStarted { .. }
            | Event::AssistantDisplayRecorded { .. }
            | Event::MessageCommitted { .. }
            | Event::RunFinished { .. } => run::apply(current, &envelope.event, prior)?,
            Event::AgentSpawned { .. }
            | Event::ProviderFallbackUsed { .. }
            | Event::ProviderRetryScheduled { .. }
            | Event::ProviderCallRecorded { .. }
            | Event::AgentFinished { .. } => agent::apply(current, &envelope.event)?,
            Event::WorkerDataChunk { .. }
            | Event::WorkerDataSealed { .. }
            | Event::WorkerStopObserved { .. }
            | Event::WorkerHandoverRecorded { .. }
            | Event::WorkerIntegrationRecorded { .. }
            | Event::WorkerHandoverDiscarded { .. }
            | Event::WorkerLeaseReleased { .. }
            | Event::WorkerVerificationRecorded { .. } => {
                worker::apply(current, &envelope.event, prior)?
            }
            Event::ToolStarted { .. }
            | Event::ToolFinished { .. }
            | Event::ToolPermissionDecided { .. }
            | Event::HookStarted { .. }
            | Event::HookFinished { .. } => tool::apply(current, &envelope.event, prior)?,
        }
        if let Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            ..
        } = &envelope.event
        {
            current
                .runs
                .iter_mut()
                .find(|run| run.id == *run_id)
                .ok_or(ReplayError::InvalidTransition)?
                .provider_sequences
                .insert(*agent_run_id, prior);
        }
        worker::after_event(current, &envelope.event, prior)?;
        crate::supervisor::SupervisorState::after(current, envelope);
        if let Event::RunStarted { run_id, .. } = envelope.event {
            current
                .rewinds
                .checkpoints
                .push(super::rewind::CheckpointTarget::Run {
                    run_id,
                    sequence: prior,
                });
        }
        current.rewinds.prefix.update(envelope)?;
        current.last_sequence = prior;
        Ok(())
    }

    pub(crate) fn previous_subject_sequence(&self) -> u64 {
        self.runs.last().map_or(self.last_sequence, |r| {
            r.supervisor.previous_subject(self.last_sequence)
        })
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
        if super::rewind::goal_blocked(self, latest) {
            return None;
        }
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
            disposition: AgentDisposition::Failed
                | AgentDisposition::Cancelled
                | AgentDisposition::StoppedByHook,
            ..
        } | Event::RunFinished {
            disposition: RunDisposition::Failed
                | RunDisposition::Cancelled
                | RunDisposition::StoppedByHook,
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

type DefinitionMap = std::collections::BTreeMap<String, crate::tools::definitions::AgentDefinition>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DefinitionCache(Option<std::sync::Arc<DefinitionMap>>);

fn definition_registry(run: &RunView) -> Result<std::sync::Arc<DefinitionMap>, ReplayError> {
    match &run.definitions.0 {
        Some(registry) => Ok(registry.clone()),
        None => compute_definition_registry(run),
    }
}

fn compute_definition_registry(
    run: &RunView,
) -> Result<std::sync::Arc<DefinitionMap>, ReplayError> {
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
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let child_model = config
        .providers
        .as_ref()
        .map_or(config.model.as_str(), |routes| {
            routes.child[0].model.as_str()
        });
    Ok(std::sync::Arc::new(
        crate::tools::definitions::registry(pin, outputs.into_iter(), child_model)
            .map_err(|_| ReplayError::InvalidTransition)?
            .definitions,
    ))
}

// Every permission mode that narrows an agent: the worker parent grant, its own grant and the definition.
fn mode_restrictions(
    run: &RunView,
    actor: AgentRunId,
) -> Result<Vec<crate::tools::permissions::PermissionMode>, ReplayError> {
    let Some(agent) = run.agents.iter().find(|agent| agent.id == actor) else {
        return Ok(Vec::new());
    };
    let Some(name) = agent.definition.as_ref() else {
        return Ok(Vec::new());
    };
    let definition = definition_registry(run)?
        .get(name)
        .ok_or(ReplayError::InvalidTransition)?
        .permission_mode;
    let (parent, grant) = agent.assignment.as_ref().map_or((None, None), |w| {
        (
            run.config
                .as_ref()
                .and_then(|c| c.worker_policy.as_ref())
                .and_then(|p| p.parent_grant.permission_mode),
            w.pins.grant.permission_mode,
        )
    });
    Ok(parent.into_iter().chain(grant).chain(definition).collect())
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
    if let Some(w) = run
        .agents
        .iter()
        .find(|a| a.id == actor)
        .and_then(|a| a.assignment.as_ref())
    {
        let parent = run
            .config
            .as_ref()
            .and_then(|c| c.worker_policy.as_ref())
            .and_then(|p| p.parent_grant.permission_mode);
        if let Some(mode) = w.pins.grant.permission_mode.or(parent) {
            return Ok(Some(mode));
        }
    }
    Ok(registry
        .get(name)
        .ok_or(ReplayError::InvalidTransition)?
        .permission_mode)
}
