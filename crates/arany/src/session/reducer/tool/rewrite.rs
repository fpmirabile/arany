use super::*;

pub(super) fn started(
    current: &mut SessionView,
    run_id: crate::session::RunId,
    agent_id: crate::session::AgentRunId,
    hook: &crate::tools::permissions::HookInvocation,
    sequence: u64,
) -> Result<(), ReplayError> {
    let ceiling = hook.active_ceiling_ms.ok_or(ReplayError::InvalidTransition)?;
    let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
    if goal
        .effective_budget()
        .max_active_ms
        .is_some_and(|limit| goal.active_ms.saturating_add(ceiling) > limit)
    {
        return Err(ReplayError::InvalidTransition);
    }
    let goal_id = goal.id;
    let run = active_run(current, run_id)?;
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let primary = run
        .primary()
        .is_some_and(|agent| agent.id == agent_id && agent.status == AgentStatus::Active);
    let pinned = config
        .tool_policy
        .as_ref()
        .is_some_and(|policy| policy.command_tools_digest.is_some());
    if !primary
        || !pinned
        || !run.hooks_settled()
        || !subject_admitted(run, hook)
        || hook.intent.workspace_device != config.workspace_device
        || hook.intent.workspace_inode != config.workspace_inode
        || hook.active_ceiling_ms.is_none_or(|ceiling| ceiling == 0)
        || hook.group as usize >= crate::tools::command_tools::MAX_COMMAND_TOOLS
        || hook.command_tool.as_ref().is_none_or(|tool| {
            !tool.enabled || !tool.valid() || hook.intent.call != tool.call()
        })
        || hook.handler != 0
        || run
            .lifecycle_hooks
            .iter()
            .any(|view| view.invocation.intent.id == hook.intent.id)
    {
        return Err(ReplayError::InvalidTransition);
    }
    run.lifecycle_hooks.push(HookView {
        sequence,
        invocation: hook.clone(),
        outcome: None,
        idle: false,
    });
    run.lifecycle_hook_sequence = Some(sequence);
    super::super::goal::record_active_ceiling(current_goal_mut(current, goal_id)?, ceiling);
    Ok(())
}

fn subject_admitted(run: &RunView, hook: &crate::tools::permissions::HookInvocation) -> bool {
    let Some(subject) = hook.command_subject else {
        return false;
    };
    let Some(pending) = super::super::pending_tool_requests(run) else {
        return false;
    };
    if pending == 0 || run.tools.iter().any(|tool| tool.intent.id == subject) {
        return false;
    }
    let mut subjects = std::collections::BTreeSet::new();
    for recorded in &run.lifecycle_hooks {
        if let Some(id) = recorded.invocation.command_subject {
            if !run.tools.iter().any(|tool| tool.intent.id == id) {
                subjects.insert(id);
            }
        }
    }
    subjects.insert(subject);
    let previous = run
        .lifecycle_hooks
        .iter()
        .rev()
        .find(|recorded| recorded.invocation.command_subject == Some(subject));
    subjects.len() <= pending
        && previous.is_none_or(|recorded| recorded.invocation.group < hook.group)
}

pub(super) fn valid(run: &RunView, intent: &EffectIntent) -> bool {
    use crate::tools::RewriteStage;
    let mut expected = intent.rewrite.first().map(|fact| fact.original_sha256);
    let mut previous = None;
    for (index, fact) in intent.rewrite.iter().enumerate() {
        if expected != Some(fact.original_sha256) {
            return false;
        }
        let Some(id) = fact.hook_intent_id else {
            if !matches!(fact.stage, RewriteStage::PinRefused | RewriteStage::InputLimit) {
                return false;
            }
            continue;
        };
        if intent.rewrite[..index]
            .iter()
            .any(|prior| prior.hook_intent_id == Some(id))
            || run
                .tools
                .iter()
                .flat_map(|tool| &tool.intent.rewrite)
                .any(|prior| prior.hook_intent_id == Some(id))
        {
            return false;
        }
        let Some(hook) = run
            .lifecycle_hooks
            .iter()
            .find(|hook| hook.invocation.intent.id == id)
        else {
            return false;
        };
        if hook.invocation.event != HookEvent::CommandRewrite
            || hook.invocation.command_subject != Some(intent.id)
            || hook.invocation.subject_digest != fact.original_sha256
            || hook
                .invocation
                .command_tool
                .as_ref()
                .is_none_or(|tool| tool.sha256 != fact.tool_sha256)
            || previous.is_some_and(|group| group >= hook.invocation.group)
        {
            return false;
        }
        previous = Some(hook.invocation.group);
        let Some(outcome) = &hook.outcome else {
            return false;
        };
        let (stage, command) = outcome.result.command_rewrite(outcome.observation.disposition);
        if let Some(command) = command {
            let call = crate::tools::command_tools::rewritten(&intent.call, command.to_owned());
            expected = Some(crate::validate::digest(
                &serde_json::to_vec(&call).expect("typed call is serializable"),
            ));
        }
        if fact.stage != stage || outcome.observation.stops_run() {
            return false;
        }
    }
    expected.is_none_or(|expected| {
        expected
            == crate::validate::digest(
                &serde_json::to_vec(&intent.call).expect("typed call is serializable"),
            )
    })
}
