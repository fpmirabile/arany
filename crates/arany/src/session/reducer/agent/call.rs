use super::super::{
    AgentStatus, AgentView, RunView, SessionView, ToolView, active_run, active_run_goal,
    all_children_finished, current_goal_mut, definition_registry, is_delegated_synthesis,
    is_tool_continuation, worker,
};
use super::{child_tool_requests, is_child_step};
use crate::provider::{AgentPhase, ProviderRole};
use crate::session::events::{
    AgentRole, AgentRunId, ChildGrant, Event, GoalId, ProviderCallDisposition, ProviderCallRecord,
    ReplayError,
};

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::ProviderCallRecorded {
        run_id,
        agent_run_id,
        record,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
    if !goal.unobserved_provider_calls.contains(agent_run_id) {
        return Err(ReplayError::InvalidTransition);
    }
    let goal_id = goal.id;
    let plan_recorded = goal.plan_contract.is_some();
    let awaiting_plan = goal.awaiting_plan_contract;
    let run = active_run(current, *run_id)?;
    let index = run
        .agents
        .iter()
        .position(|agent| agent.id == *agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let agent = &run.agents[index];
    validate_attempt_role(run, agent, *agent_run_id, record)?;
    validate_provider_cap(run, agent, *agent_run_id, record)?;
    let admission = continuation_admission(run, agent, record, plan_recorded, awaiting_plan);
    validate_disposition(run, agent, record, plan_recorded, admission)?;
    validate_display(run, agent, record)?;
    if record.phase == AgentPhase::ToolReview {
        let review_index = u32::try_from(agent.provider_calls.len())
            .map_err(|_| ReplayError::InvalidTransition)?;
        let tools = if run.agents[index].role == AgentRole::Primary {
            &mut run.tools
        } else {
            &mut run.agents[index].tools
        };
        tools
            .last_mut()
            .ok_or(ReplayError::InvalidTransition)?
            .review_call_index = Some(review_index);
    }
    let role = run.agents[index].role;
    run.agents[index].provider_calls.push(record.clone());
    charge_goal_call(current, goal_id, *agent_run_id, record, role)?;
    Ok(())
}

fn is_tool_review(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    if agent.role == AgentRole::Child {
        return record.phase == AgentPhase::ToolReview
            && matches!(
                agent.grant,
                Some(ChildGrant::ReadOnly { .. } | ChildGrant::EditOnly { .. })
            )
            && agent
                .provider_calls
                .last()
                .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            && agent.tools.last().is_some_and(|tool| {
                tool.started_sequence.is_none()
                    && tool.observation.is_none()
                    && tool.review_call_index.is_none()
                    && tool.permission_verdict.is_none()
            });
    }
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
        && run.tools.last().is_some_and(|tool| {
            tool.started_sequence.is_none()
                && tool.observation.is_none()
                && tool.review_call_index.is_none()
                && tool.permission_verdict.is_none()
        })
}

fn validate_attempt_role(
    run: &RunView,
    agent: &AgentView,
    agent_run_id: AgentRunId,
    record: &ProviderCallRecord,
) -> Result<(), ReplayError> {
    let call_index =
        u32::try_from(agent.provider_calls.len()).map_err(|_| ReplayError::InvalidTransition)?;
    if run
        .provider_fallbacks
        .iter()
        .rev()
        .find(|(actor, fallback)| {
            *actor == agent_run_id
                && fallback.role != ProviderRole::Compaction
                && fallback.call_index == call_index
        })
        .is_some_and(|(_, fallback)| fallback.role != ProviderRole::for_phase(record.phase))
        || run.provider_retries.iter().any(|(actor, retry)| {
            *actor == agent_run_id
                && retry.role != ProviderRole::Compaction
                && retry.call_index == call_index
                && retry.role != ProviderRole::for_phase(record.phase)
        })
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn validate_provider_cap(
    run: &RunView,
    agent: &AgentView,
    agent_run_id: AgentRunId,
    record: &ProviderCallRecord,
) -> Result<(), ReplayError> {
    let role = ProviderRole::for_phase(record.phase);
    let inherited = run.config.as_ref().and_then(|config| config.identity());
    if let Some(initial) = run
        .provider_chain(agent_run_id, role)
        .and_then(|chain| chain.first())
        .or(inherited.as_ref())
    {
        let identity = run.current_identity(agent_run_id, role, initial);
        let cap = match role {
            ProviderRole::Primary => identity.limits.max_output_tokens,
            ProviderRole::Child if agent.assignment.is_some() => run
                .provider_fallbacks
                .iter()
                .filter(|(actor, fallback)| {
                    *actor == agent_run_id
                        && fallback.role == role
                        && usize::try_from(fallback.call_index).ok()
                            == Some(agent.provider_calls.len())
                })
                .fold(identity.limits.max_output_tokens, |cap, (_, fallback)| {
                    cap.min(fallback.from.limits.max_output_tokens)
                }),
            ProviderRole::Child => identity
                .limits
                .max_output_tokens
                .min(crate::provider::DEFAULT_OUTPUT_TOKEN_CAP),
            ProviderRole::Review => identity.limits.max_output_tokens.min(256),
            ProviderRole::Compaction => {
                return Err(ReplayError::InvalidTransition);
            }
        };
        if !identity
            .limits
            .output_token_bound
            .admits_reported_output(record.output_tokens, cap)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

struct ContinuationAdmission {
    tools_enabled: bool,
    valid: bool,
}

fn continuation_admission(
    run: &RunView,
    agent: &AgentView,
    record: &ProviderCallRecord,
    plan_recorded: bool,
    awaiting_plan: bool,
) -> ContinuationAdmission {
    let tools_enabled = run
        .config
        .as_ref()
        .is_some_and(|config| config.tool_policy.is_some());
    let tool_continuation = tools_enabled && is_tool_continuation(run, agent, record);
    let delegated = tools_enabled && is_delegated_synthesis(run, agent, record);
    let tool_review = tools_enabled && is_tool_review(run, agent, record);
    let stop_continuation = run.stop_blocked(agent.id)
        && agent
            .provider_calls
            .last()
            .is_some_and(|call| call.phase == record.phase)
        && run.hooks_settled();
    let gate_continuation = agent.assignment.is_some()
        && agent.provider_calls.last().is_some_and(|c| {
            c.failure_reason == Some(crate::provider::ProviderFailureReason::CriterionNotMetInView)
        })
        && record.phase == AgentPhase::ChildWork;
    let refusal_continuation = agent.role == AgentRole::Primary
        && !agent.refusal_limit_reached()
        && agent
            .provider_calls
            .last()
            .is_some_and(|call| call.refusal.is_some() && call.phase == record.phase)
        && run.hooks_settled()
        && run.tools.iter().all(ToolView::settled_without_stop);
    let plan_continuation = tools_enabled
        && agent.role == AgentRole::Primary
        && agent.provider_calls.last().is_some_and(|call| {
            call.disposition == ProviderCallDisposition::PlanRecorded && call.phase == record.phase
        })
        && (plan_recorded && !awaiting_plan)
        && run.hooks_settled();
    let outcome_correction = agent.role == AgentRole::Primary
        && agent.outcome_correction_pending()
        && agent
            .provider_calls
            .last()
            .is_some_and(|call| call.phase == record.phase)
        && run.hooks_settled()
        && run.tools.iter().all(ToolView::settled_without_stop);
    let valid = outcome_correction
        || plan_continuation
        || refusal_continuation
        || gate_continuation
        || stop_continuation
        || tool_continuation
        || tool_review
        || delegated
        || match (agent.role, agent.provider_calls.as_slice(), record.phase) {
            (AgentRole::Primary, [], AgentPhase::RootPlan) => true,
            (AgentRole::Primary, calls, AgentPhase::RootSynthesis)
                if calls
                    .last()
                    .is_some_and(|call| call.disposition == ProviderCallDisposition::Delegated) =>
            {
                all_children_finished(run)
                    || record.disposition == ProviderCallDisposition::ToolRequested
                        && worker::coordination_ready(run)
            }
            (AgentRole::Child, _, AgentPhase::ChildWork) => is_child_step(agent, record),
            _ => false,
        };
    ContinuationAdmission {
        tools_enabled,
        valid,
    }
}

fn tool_request_fits(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    match agent.role {
        AgentRole::Primary => record.phase != AgentPhase::ToolReview,
        AgentRole::Child => {
            record.phase == AgentPhase::ChildWork
                && matches!(
                    agent.grant,
                    Some(ChildGrant::ReadOnly { .. } | ChildGrant::EditOnly { .. })
                )
                && record.requested_tools == 1
                && child_definition_allows_turn(run, agent)
                && child_tool_requests(agent) < crate::session::MAX_CHILD_TOOL_CALLS
        }
    }
}

fn validate_disposition(
    run: &RunView,
    agent: &AgentView,
    record: &ProviderCallRecord,
    plan_recorded: bool,
    admission: ContinuationAdmission,
) -> Result<(), ReplayError> {
    if agent.status != AgentStatus::Active
        || agent.role == AgentRole::Primary && agent.outcome_correction_exhausted()
        || !admission.valid
        || record.disposition == ProviderCallDisposition::PlanRecorded
            && !(admission.tools_enabled
                && agent.role == AgentRole::Primary
                && record.phase != AgentPhase::ToolReview
                && !plan_recorded)
        || record.disposition == ProviderCallDisposition::ToolRequested
            && !(admission.tools_enabled && tool_request_fits(run, agent, record))
        || (record.phase != AgentPhase::RootPlan
            && !(record.phase == AgentPhase::RootSynthesis && worker::wave_settled(run))
            && record.disposition == ProviderCallDisposition::Delegated)
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn validate_display(
    run: &RunView,
    agent: &AgentView,
    record: &ProviderCallRecord,
) -> Result<(), ReplayError> {
    if agent.role == AgentRole::Primary
        && matches!(
            record.disposition,
            ProviderCallDisposition::Delegated
                | ProviderCallDisposition::ToolRequested
                | ProviderCallDisposition::PlanRecorded
        )
        && run.display.iter().any(|chunk| {
            chunk.category == crate::session::events::DisplayCategory::Answer
                && usize::try_from(chunk.call_index).ok() == Some(agent.provider_calls.len())
        })
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn charge_goal_call(
    current: &mut SessionView,
    goal_id: GoalId,
    agent_run_id: AgentRunId,
    record: &ProviderCallRecord,
    role: AgentRole,
) -> Result<(), ReplayError> {
    let goal = current_goal_mut(current, goal_id)?;
    let reservation_index = goal
        .unobserved_provider_calls
        .iter()
        .position(|reserved| *reserved == agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    goal.unobserved_provider_calls.remove(reservation_index);
    goal.known_tokens = goal
        .known_tokens
        .checked_add(u64::from(record.input_tokens.unwrap_or_default()))
        .and_then(|tokens| tokens.checked_add(u64::from(record.output_tokens.unwrap_or_default())))
        .ok_or(ReplayError::InvalidTransition)?;
    goal.known_cached_tokens = goal
        .known_cached_tokens
        .checked_add(u64::from(
            record.cache_read_input_tokens.unwrap_or_default(),
        ))
        .ok_or(ReplayError::InvalidTransition)?;
    goal.token_usage_unknown |= record.input_tokens.is_none() || record.output_tokens.is_none();
    if role == AgentRole::Primary
        && record.disposition == ProviderCallDisposition::PlanRecorded
        && goal.plan_contract.is_none()
    {
        if goal.awaiting_plan_contract {
            return Err(ReplayError::InvalidTransition);
        }
        goal.awaiting_plan_contract = true;
    }
    Ok(())
}

fn child_definition_allows_turn(run: &RunView, agent: &AgentView) -> bool {
    agent.definition.as_ref().is_none_or(|name| {
        definition_registry(run)
            .ok()
            .and_then(|registry| registry.get(name).cloned())
            .is_some_and(|definition| {
                agent
                    .provider_calls
                    .iter()
                    .filter(|prior| prior.phase == AgentPhase::ChildWork)
                    .count()
                    + 1
                    < usize::from(definition.max_turns)
            })
    })
}
