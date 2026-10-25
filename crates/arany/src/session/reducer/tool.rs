use super::definition_mode;

mod hook;
mod lifecycle;
mod permission;
use super::{
    AgentStatus, HookView, MAX_HOST_SESSION_GRANTS, RunView, SessionView, ToolView, active_run,
    active_run_goal, current_goal_mut, observed_command_changeset, pending_tool_requests,
};
use crate::provider::AgentPhase;
use crate::session::events::{Event, ProviderCallDisposition, ReplayError};
use crate::tools::permissions::{
    Decision, HookEvent, HookResult, HostApproval, MAX_HOOKS_PER_OCCURRENCE, PermissionMode,
    PermissionSource, PermissionVerdict,
};
use crate::tools::{EffectIntent, ToolPolicyReceipt};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    lifecycle::validate_finished(current, event)?;
    match event {
        Event::ToolPermissionDecided { .. } => permission::apply(current, event)?,
        Event::HookStarted {
            run_id,
            agent_run_id,
            hook,
        } => hook::started(current, *run_id, *agent_run_id, hook, prior)?,
        Event::HookFinished {
            run_id,
            agent_run_id,
            outcome,
        } => hook::finished(current, *run_id, *agent_run_id, outcome, prior)?,
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
            active_ceiling_ms,
        } => lifecycle::started(current, *run_id, *agent_run_id, intent, *active_ceiling_ms)?,
        Event::ToolFinished { .. } => lifecycle::finished(current, event, prior)?,
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

fn permissions_pinned(run: &RunView) -> bool {
    run.config
        .as_ref()
        .and_then(|config| config.tool_policy.as_ref())
        .is_some_and(|policy| policy.permissions_digest.is_some())
}

// The Tool a PostToolUse hook follows: the last Tool, or the source of the changeset the last Tool
// integrated. The hook must directly follow that Tool's latest journaled step.
fn post_target<'a>(
    run: &'a mut RunView,
    actor: crate::session::AgentRunId,
    subject: &[u8; 32],
    previous: u64,
) -> Result<(&'a mut ToolView, bool), ReplayError> {
    let tools = if run
        .primary()
        .is_some_and(|agent| agent.id == actor && agent.status == AgentStatus::Active)
    {
        &mut run.tools
    } else {
        &mut run
            .agents
            .iter_mut()
            .find(|agent| agent.id == actor && agent.status == AgentStatus::Active)
            .ok_or(ReplayError::InvalidTransition)?
            .tools
    };
    let last = tools
        .len()
        .checked_sub(1)
        .ok_or(ReplayError::InvalidTransition)?;
    let integration = &tools[last];
    if matches!(
        integration.intent.call,
        crate::tools::ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt { .. }
        }
    ) && integration.intent.digest() == *subject
    {
        return Err(ReplayError::InvalidTransition);
    }

    let integrated = matches!(
        &integration.intent.call,
        crate::tools::ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest,
                ..
            },
        } if source_intent_digest == subject
    ) && last > 0
        && integration.observation.as_ref().is_some_and(|observation| {
            observation.disposition == crate::tools::ToolDisposition::Succeeded
        });
    let index = if integrated { last - 1 } else { last };
    let awaiting_integration = !integrated
        && integration
            .observation
            .as_ref()
            .and_then(super::observed_command_changeset)
            .is_some_and(|changeset| !changeset.files.is_empty());
    let capture_sequence = integration.observation.as_ref().and_then(|o| {
        if !matches!(
            o.attestation,
            Some(crate::tools::Attestation::WorkerCommand { .. })
        ) {
            return None;
        }
        let result = crate::worker::WorkerCommandResult::parse(&o.output)?;
        match result.private_capture {
            crate::worker::PrivateCapture::Sealed { data } => Some(data.seal_sequence),
            _ => None,
        }
    });
    let tail = integration
        .finished_sequence
        .max(tools[index].hook_sequence)
        .max(capture_sequence);
    let adjacent = !awaiting_integration && tail == Some(previous);
    Ok((&mut tools[index], adjacent))
}

fn pending_tool(
    run: &mut RunView,
    actor: crate::session::AgentRunId,
) -> Result<&mut ToolView, ReplayError> {
    if run
        .primary()
        .is_some_and(|primary| primary.id == actor && primary.status == AgentStatus::Active)
    {
        run.tools.last_mut()
    } else {
        run.agents
            .iter_mut()
            .find(|agent| agent.id == actor && agent.status == AgentStatus::Active)
            .and_then(|agent| agent.tools.last_mut())
    }
    .ok_or(ReplayError::InvalidTransition)
}

// A narrowed Tool answers nothing itself: the next Tool must be exactly its replacement.
fn valid_narrowing_source(run: &RunView, intent: &crate::tools::EffectIntent) -> bool {
    run.tools
        .last()
        .and_then(ToolView::narrowed_call)
        .is_none_or(|call| *call == intent.call)
}

fn valid_integration_source(run: &RunView, intent: &crate::tools::EffectIntent) -> bool {
    if let crate::tools::ToolCall::ApplyChangeset {
        source: source @ crate::session::worker::ChangesetSource::WorkerHandover { .. },
    } = &intent.call
    {
        return super::worker::integration_source(run, source);
    }
    let crate::tools::ToolCall::ApplyChangeset {
        source:
            crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest,
                changeset,
            },
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

fn record_run_ceiling(
    current: &mut SessionView,
    run_id: crate::session::RunId,
    ceiling_ms: u64,
) -> Result<(), ReplayError> {
    let goal_id = active_run_goal(current, run_id)
        .ok_or(ReplayError::InvalidTransition)?
        .id;
    super::goal::record_active_ceiling(current_goal_mut(current, goal_id)?, ceiling_ms);
    Ok(())
}

fn start_lifecycle(
    current: &mut SessionView,
    run_id: crate::session::RunId,
    agent_id: crate::session::AgentRunId,
    hook: &crate::tools::permissions::HookInvocation,
    sequence: u64,
) -> Result<(), ReplayError> {
    use crate::session::{AgentRole, ChildGrant, RunStatus};
    use crate::tools::permissions::{MAX_STOP_CONTINUATIONS, compact_subject, stop_subject};
    let previous = current.previous_subject_sequence();
    let run = current
        .runs
        .last_mut()
        .filter(|run| run.id == run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let agent = run
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or(ReplayError::InvalidTransition)?;
    if !permissions_pinned(run)
        || !run.hooks_settled()
        || hook.intent.policy_digest
            != config
                .tool_policy
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?
                .config_digest
        || hook.intent.workspace_device != config.workspace_device
        || hook.intent.workspace_inode != config.workspace_inode
    {
        return Err(ReplayError::InvalidTransition);
    }
    let count = agent.provider_calls.len();
    let same: Vec<_> = run
        .lifecycle_hooks
        .iter()
        .filter(|view| {
            view.invocation.event == hook.event
                && view.invocation.subject_digest == hook.subject_digest
                && view.invocation.intent.agent_run_id == agent_id
        })
        .collect();
    if same.len() >= MAX_HOOKS_PER_OCCURRENCE
        || same.iter().any(|hook| {
            hook.outcome
                .as_ref()
                .is_some_and(|outcome| matches!(outcome.result, HookResult::Blocked { .. }))
        })
        || run
            .lifecycle_hooks
            .iter()
            .any(|view| view.invocation.intent.id == hook.intent.id)
    {
        return Err(ReplayError::InvalidTransition);
    }
    let adjacent = if same.is_empty() {
        run.provider_sequences.get(&agent_id).copied()
    } else {
        run.lifecycle_hook_sequence
    } == Some(previous);
    match hook.event {
        HookEvent::Stop | HookEvent::SubagentStop => {
            let role_matches = if hook.event == HookEvent::Stop {
                agent.role == AgentRole::Primary
            } else {
                agent.role == AgentRole::Child
                    && matches!(
                        (agent.grant, agent.assignment.is_some()),
                        (Some(ChildGrant::ReadOnly { .. }), false)
                            | (Some(ChildGrant::EditOnly { .. }), true)
                    )
            };
            if run.status != RunStatus::Active
                || agent.status != AgentStatus::Active
                || !role_matches
                || !adjacent
                || count == 0
                || hook.subject_digest != stop_subject(agent_id, count - 1)
                || agent.provider_calls.last().is_none_or(|call| {
                    call.disposition != ProviderCallDisposition::Finished
                        || call.phase == AgentPhase::ToolReview
                })
                || run.stop_continuations(agent_id) >= MAX_STOP_CONTINUATIONS
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
        HookEvent::PreCompact => {
            let pending = current
                .goals
                .iter()
                .find(|goal| goal.run_ids.last() == Some(&run_id))
                .is_some_and(|goal| {
                    !goal.unobserved_provider_calls.is_empty()
                        || goal.compaction_call_reserved.is_some()
                });
            if pending
                || run.tools.iter().any(|tool| tool.observation.is_none())
                || agent.role != AgentRole::Primary
                || !(run.status == RunStatus::Active || run.status.is_outcome())
                || !["manual", "auto"]
                    .iter()
                    .any(|trigger| hook.subject_digest == compact_subject(trigger, count))
                || !same.is_empty() && run.lifecycle_hook_sequence != Some(previous)
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    let boundary = run.status != RunStatus::Active;
    let goal = current.goals.iter_mut().find(|goal| {
        goal.status == crate::session::GoalStatus::Running && goal.run_ids.last() == Some(&run_id)
    });
    if hook.active_ceiling_ms.is_some() != goal.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    if let Some(ceiling) = hook.active_ceiling_ms {
        let goal = goal.ok_or(ReplayError::InvalidTransition)?;
        if ceiling == 0
            || goal
                .effective_budget()
                .max_active_ms
                .is_some_and(|limit| goal.active_ms.saturating_add(ceiling) > limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        if boundary && goal.active_run.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
        super::goal::record_active_ceiling(goal, ceiling);
    }
    run.lifecycle_hooks.push(HookView {
        sequence,
        invocation: hook.clone(),
        outcome: None,
        idle: boundary,
    });
    run.lifecycle_hook_sequence = Some(sequence);
    Ok(())
}

// A crash leaves a lifecycle hook open; the next Run interrupts it as it does an open Tool hook.
fn opens_other_run(run: &RunView, event: &Event) -> bool {
    match event {
        Event::GoalRunLinked { run_id, .. } | Event::MessageAccepted { run_id, .. } => {
            *run_id != run.id
        }
        _ => false,
    }
}

pub(super) fn before_lifecycle_event(
    current: &SessionView,
    event: &Event,
) -> Result<(), ReplayError> {
    let Some(run) = current.runs.last() else {
        return Ok(());
    };
    if run
        .lifecycle_hooks
        .last()
        .is_some_and(|hook| hook.outcome.is_none())
        && !matches!(
            event,
            Event::HookFinished { .. }
                | Event::GoalStarted { .. }
                | Event::GoalResumed { .. }
                | Event::GoalFinished { .. }
                | Event::SessionRenamed { .. }
                | Event::ProviderSetupEditStarted { .. }
                | Event::ProviderSetupEditFinished { .. }
                | Event::WorkspaceRewindStarted { .. }
                | Event::WorkspaceRewindFinished { .. }
                | Event::SessionDefaultChanged { .. }
        )
        && !opens_other_run(run, event)
    {
        return Err(ReplayError::InvalidTransition);
    }
    if run.lifecycle_hooks.iter().any(|hook| {
        hook.outcome
            .as_ref()
            .is_some_and(|outcome| outcome.observation.stops_run())
    }) && matches!(
        event,
        Event::GoalProviderCallReserved { .. }
            | Event::ToolStarted { .. }
            | Event::HookStarted { .. }
            | Event::AgentSpawned { .. }
    ) {
        return Err(ReplayError::InvalidTransition);
    }
    if matches!(
        event,
        Event::GoalCompactionCallReserved { .. } | Event::ContextCompacted { .. }
    ) {
        if !run.hooks_settled() {
            return Err(ReplayError::InvalidTransition);
        }
        let count = run.primary().map_or(0, |agent| agent.provider_calls.len());
        if run.lifecycle_hooks.iter().any(|hook| {
            hook.invocation.event == HookEvent::PreCompact
                && ["manual", "auto"].iter().any(|trigger| {
                    hook.invocation.subject_digest
                        == crate::tools::permissions::compact_subject(trigger, count)
                })
                && hook
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| matches!(outcome.result, HookResult::Blocked { .. }))
        }) {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

fn valid_host_approval(
    view: &SessionView,
    run_id: crate::session::RunId,
    actor: crate::session::AgentRunId,
    verdict: &PermissionVerdict,
) -> bool {
    let Some(run) = view.runs.last().filter(|run| run.id == run_id) else {
        return false;
    };
    if run.primary().is_none_or(|agent| agent.id != actor) {
        return false;
    }
    let Some(tool) = run.tools.last() else {
        return false;
    };
    let Some(policy) = run
        .config
        .as_ref()
        .and_then(|config| config.tool_policy.as_ref())
    else {
        return false;
    };
    let Some(host) = &verdict.host else {
        return false;
    };
    let Some(pin) = &tool.intent.host_command else {
        return false;
    };
    let crate::tools::ToolCall::CommandUnguarded { program, args, .. } = &tool.intent.call else {
        return false;
    };
    if policy.unguarded != crate::tools::UnguardedGrant::HumanOnly
        || policy.host_epoch != Some(host.epoch)
        || host.epoch.get_version() != Some(uuid::Version::SortRand)
        || verdict.decision == Decision::Deny
        || !pin.valid()
        || crate::tools::unguarded::prohibited(program, args)
        || crate::tools::unguarded::prohibited(&pin.executable, args)
    {
        return false;
    }
    match verdict.source {
        PermissionSource::Human => valid_human_host_approval(view, &tool.intent, verdict, host),
        PermissionSource::SessionApproval => {
            valid_session_host_approval(view, &tool.intent, verdict, host)
        }
        PermissionSource::HostPrefixRule => {
            valid_host_prefix_approval(policy, &tool.intent, verdict, host)
        }
        PermissionSource::Rule
        | PermissionSource::Mode
        | PermissionSource::AiReview
        | PermissionSource::UnavailableApproval
        | PermissionSource::PinnedGrant
        | PermissionSource::Hook
        | PermissionSource::TypedPrefixRule => false,
    }
}

fn valid_human_host_approval(
    view: &SessionView,
    intent: &EffectIntent,
    verdict: &PermissionVerdict,
    host: &HostApproval,
) -> bool {
    host.rule.as_ref().is_none_or(|rule| {
        let crate::tools::ToolCall::CommandUnguarded {
            prefix_rule: Some(prefix),
            ..
        } = &intent.call
        else {
            return false;
        };
        !host.session
            && rule.decision == Decision::Allow
            && matches!(verdict.mode, None | Some(PermissionMode::Default))
            && verdict.basis == PermissionSource::Mode
            && verdict.hook_restriction.is_none()
            && rule.matches(intent)
            && prefix.len() >= crate::tools::permissions::MIN_HOST_PREFIX_TOKENS
            && *prefix == rule.prefix
    }) && host.source_intent.is_none()
        && verdict.basis != PermissionSource::SessionApproval
        && verdict.decision == Decision::Ask
        && (!host.session
            || matches!(verdict.mode, None | Some(PermissionMode::Default))
                && view.host_grants(host.epoch).count() < MAX_HOST_SESSION_GRANTS)
}

fn valid_session_host_approval(
    view: &SessionView,
    intent: &EffectIntent,
    verdict: &PermissionVerdict,
    host: &HostApproval,
) -> bool {
    host.rule.is_none()
        && !host.session
        && verdict.decision == Decision::Allow
        && intent
            .host_command
            .as_ref()
            .is_some_and(|pin| !crate::tools::unguarded::state_target(&intent.call, pin))
        && matches!(
            verdict.mode,
            None | Some(PermissionMode::Default | PermissionMode::DontAsk)
        )
        && verdict.basis == PermissionSource::SessionApproval
        && verdict.hook_restriction.is_none()
        && host.source_intent.is_some()
        && view.host_approval(intent, host.epoch) == host.source_intent
}

fn valid_host_prefix_approval(
    policy: &ToolPolicyReceipt,
    intent: &EffectIntent,
    verdict: &PermissionVerdict,
    host: &HostApproval,
) -> bool {
    let Some(rule) = &host.rule else {
        return false;
    };
    !host.session
        && host.source_intent.is_none()
        && verdict.decision == Decision::Allow
        && verdict.basis == PermissionSource::HostPrefixRule
        && verdict.hook_restriction.is_none()
        && policy.permissions_digest.is_some()
        && intent
            .host_command
            .as_ref()
            .is_some_and(|pin| policy.permissions_digest == pin.permissions_digest)
        && matches!(
            verdict.mode,
            Some(PermissionMode::Default | PermissionMode::DontAsk)
        )
        && rule.decision == Decision::Allow
        && rule.matches(intent)
        && policy.host_prefixes.contains(rule)
        && !policy
            .host_prefixes
            .iter()
            .any(|other| other.decision != Decision::Allow && other.scope_matches(intent))
}
