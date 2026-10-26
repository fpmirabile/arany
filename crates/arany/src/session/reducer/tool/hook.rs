use super::{
    HookView, SessionView, active_run, permissions_pinned, post_target, start_lifecycle,
    tool_by_digest,
};
use crate::session::events::ReplayError;
use crate::session::{AgentRunId, GoalStatus, RunId};
use crate::tools::permissions::{HookEvent, HookInvocation, HookOutcome, HookResult};

pub(super) fn started(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    hook: &HookInvocation,
    prior: u64,
) -> Result<(), ReplayError> {
    if !matches!(hook.event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
        return start_lifecycle(current, run_id, agent_run_id, hook, prior);
    }
    let previous = current.previous_subject_sequence();
    let run = active_run(current, run_id)?;
    if !permissions_pinned(run) || hook.intent.agent_run_id != agent_run_id {
        return Err(ReplayError::InvalidTransition);
    }
    if hook.active_ceiling_ms.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    let post = hook.event == HookEvent::PostToolUse;
    let (tool, state_allows) = if post {
        let (tool, adjacent) = post_target(run, agent_run_id, &hook.subject_digest, previous)?;
        let succeeded = tool.observation.as_ref().is_some_and(|observation| {
            observation.disposition == crate::tools::ToolDisposition::Succeeded
        });
        (tool, adjacent && succeeded)
    } else {
        let tool = tool_by_digest(run, agent_run_id, &hook.subject_digest)?;
        let fresh = tool.started_sequence.is_none()
            && tool.permission_verdict.is_none()
            && tool.observation.is_none();
        (tool, fresh)
    };
    let finished_restriction = tool.hooks.iter().any(|view| {
        view.outcome.as_ref().is_none_or(|outcome| {
            outcome.result.restriction(&tool.intent.call)
                == crate::tools::permissions::Decision::Deny
                || outcome.observation.stops_run()
        })
    });
    let same_event = tool
        .hooks
        .iter()
        .filter(|view| view.invocation.event == hook.event)
        .count();
    if !state_allows
        || tool.intent.call.is_internal()
        || tool.intent.agent_run_id != agent_run_id
        || hook.subject_digest != tool.intent.digest()
        || hook.intent.policy_digest != tool.intent.policy_digest
        || hook.intent.workspace_device != tool.intent.workspace_device
        || hook.intent.workspace_inode != tool.intent.workspace_inode
        || hook.intent.id == tool.intent.id
        || same_event >= crate::tools::permissions::MAX_HOOKS_PER_OCCURRENCE
        || finished_restriction
        || tool
            .hooks
            .iter()
            .any(|view| view.invocation.intent.id == hook.intent.id)
    {
        return Err(ReplayError::InvalidTransition);
    }
    tool.hooks.push(HookView {
        sequence: prior,
        invocation: hook.clone(),
        outcome: None,
        idle: false,
    });
    if post {
        tool.hook_sequence = Some(prior);
    }
    Ok(())
}

pub(super) fn finished(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    outcome: &HookOutcome,
    prior: u64,
) -> Result<(), ReplayError> {
    if current
        .runs
        .iter()
        .find(|run| run.id == run_id)
        .is_some_and(|run| {
            run.lifecycle_hooks
                .last()
                .is_some_and(|hook| hook.outcome.is_none())
        })
    {
        let run = current
            .runs
            .iter_mut()
            .find(|run| run.id == run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        let hook = run
            .lifecycle_hooks
            .last_mut()
            .ok_or(ReplayError::InvalidTransition)?;
        if hook.invocation.intent != outcome.observation.intent
            || hook.invocation.intent.agent_run_id != agent_run_id
            || outcome.result != HookResult::for_event(hook.invocation.event, &outcome.observation)
        {
            return Err(ReplayError::InvalidTransition);
        }
        if !hook.idle && run.status != crate::session::RunStatus::Active {
            return Err(ReplayError::InvalidTransition);
        }
        let ceiling = if hook.idle {
            hook.invocation.active_ceiling_ms
        } else {
            None
        };
        if outcome.boundary_active_ms.is_some() != ceiling.is_some()
            || outcome
                .boundary_active_ms
                .zip(ceiling)
                .is_some_and(|(active, ceiling)| active > ceiling)
        {
            return Err(ReplayError::InvalidTransition);
        }
        hook.outcome = Some(outcome.clone());
        run.lifecycle_hook_sequence = Some(prior);
        if let Some(active) = outcome.boundary_active_ms {
            let goal = current
                .goals
                .iter_mut()
                .find(|goal| {
                    goal.status == GoalStatus::Running && goal.run_ids.last() == Some(&run_id)
                })
                .ok_or(ReplayError::InvalidTransition)?;
            goal.active_ms = goal
                .active_ms
                .checked_add(active)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.active_ceiling_ms = 0;
        }
        return Ok(());
    }
    let previous = current.previous_subject_sequence();
    let run = active_run(current, run_id)?;
    if !permissions_pinned(run) || outcome.boundary_active_ms.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    let tool = if run.primary().is_some_and(|agent| agent.id == agent_run_id) {
        run.tools
            .iter_mut()
            .rev()
            .find(|tool| tool.hooks.last().is_some_and(|view| view.outcome.is_none()))
    } else {
        run.agents
            .iter_mut()
            .find(|agent| agent.id == agent_run_id)
            .and_then(|agent| {
                agent
                    .tools
                    .iter_mut()
                    .find(|tool| tool.hooks.last().is_some_and(|view| view.outcome.is_none()))
            })
    }
    .ok_or(ReplayError::InvalidTransition)?;
    let observed = tool.observation.is_some();
    let sequence = tool.hook_sequence;
    let view = tool
        .hooks
        .last_mut()
        .filter(|view| {
            view.outcome.is_none()
                && outcome.observation.intent.agent_run_id == agent_run_id
                && view.invocation.intent == outcome.observation.intent
                && (view.invocation.event == HookEvent::PostToolUse) == observed
                && (!observed || sequence == Some(previous))
                && outcome.result
                    == HookResult::for_event(view.invocation.event, &outcome.observation)
        })
        .ok_or(ReplayError::InvalidTransition)?;
    view.outcome = Some(outcome.clone());
    if observed {
        tool.hook_sequence = Some(prior);
    }
    Ok(())
}
