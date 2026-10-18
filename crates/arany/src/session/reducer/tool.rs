use super::agent::names_child;
use super::{
    AgentStatus, HookView, RunView, SessionView, ToolView, active_run, active_run_goal,
    current_goal_mut, observed_command_changeset, pending_tool_requests,
};
use crate::provider::AgentPhase;
use crate::session::events::{Event, GoalPhase, ProviderCallDisposition, ReplayError};
use crate::tools::permissions::{Decision, HookEvent, HookResult, MAX_HOOKS_PER_OCCURRENCE};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    if let Event::ToolFinished {
        checkpoint,
        run_id,
        agent_run_id,
        observation,
    } = event
    {
        super::super::checkpoint::validate_finished(current, observation, checkpoint.as_ref())?;
        let run = active_run(current, *run_id)?;
        let configured = run
            .config
            .as_ref()
            .and_then(|config| config.tool_policy.as_ref())
            .is_some_and(|policy| policy.permissions_digest.is_some());
        let definition_mode = super::definition_mode(run, *agent_run_id)?;
        let restrictions = super::mode_restrictions(run, *agent_run_id)?;
        let unguarded = observation.intent.call.is_unguarded();
        if configured || definition_mode.is_some() || unguarded {
            let tool = pending_tool(run, *agent_run_id)?;
            let verdict = tool
                .permission_verdict
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            for mode in restrictions {
                let restriction = crate::tools::permissions::PermissionConfig::evaluate_mode(
                    mode,
                    &observation.intent.call,
                )
                .0;
                if verdict.approved
                    && (restriction == crate::tools::permissions::Decision::Deny
                        || restriction == crate::tools::permissions::Decision::Ask
                            && !matches!(
                                verdict.source,
                                crate::tools::permissions::PermissionSource::Human
                                    | crate::tools::permissions::PermissionSource::AiReview
                            ))
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            if unguarded
                && !matches!(
                    observation.attestation,
                    None | Some(crate::tools::Attestation::Unguarded(_))
                )
            {
                return Err(ReplayError::InvalidTransition);
            }
            if !verdict.approved
                && (observation.attestation.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
                    || !matches!(
                        observation.disposition,
                        crate::tools::ToolDisposition::Denied
                            | crate::tools::ToolDisposition::Limit
                    ))
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
    }
    match event {
        Event::ToolPermissionDecided {
            run_id,
            agent_run_id,
            verdict,
        } => {
            let run = active_run(current, *run_id)?;
            let definition_mode = super::definition_mode(run, *agent_run_id)?;
            let policy = run
                .config
                .as_ref()
                .and_then(|config| config.tool_policy.as_ref());
            let permissions_pinned =
                policy.is_some_and(|policy| policy.permissions_digest.is_some());
            let rules_pinned = policy.is_some_and(|policy| policy.unguarded_rules_digest.is_some());
            let pending_call = pending_tool(run, *agent_run_id)?.intent.call.clone();
            let unguarded = pending_call.is_unguarded();
            if verdict.mode.is_some() != (permissions_pinned || definition_mode.is_some())
                || !(permissions_pinned || definition_mode.is_some() || unguarded)
                || definition_mode == Some(crate::tools::permissions::PermissionMode::DontAsk)
                    && verdict.decision == crate::tools::permissions::Decision::Ask
            {
                return Err(ReplayError::InvalidTransition);
            }
            if unguarded
                && verdict.approved
                && (!matches!(
                    verdict.source,
                    crate::tools::permissions::PermissionSource::Human
                        | crate::tools::permissions::PermissionSource::ExactCommandRule
                ) || verdict.decision == crate::tools::permissions::Decision::Deny
                    || run.primary().is_none_or(|agent| agent.id != *agent_run_id)
                    || crate::tools::types::qualifying_guard_failure(
                        &pending_call,
                        run.tools
                            .iter()
                            .filter_map(|tool| tool.observation.as_ref()),
                    )
                    .is_none())
            {
                return Err(ReplayError::InvalidTransition);
            }
            if verdict.source == crate::tools::permissions::PermissionSource::AiReview
                && verdict.approved
            {
                let index = pending_tool(run, *agent_run_id)?
                    .review_call_index
                    .ok_or(ReplayError::InvalidTransition)?;
                let reviewed = run
                    .agents
                    .iter()
                    .find(|agent| agent.id == *agent_run_id)
                    .and_then(|agent| agent.provider_calls.get(index as usize));
                if reviewed.is_none_or(|call| {
                    call.phase != AgentPhase::ToolReview
                        || call.disposition != ProviderCallDisposition::Finished
                }) {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            let chained = run
                .tools
                .len()
                .checked_sub(2)
                .is_some_and(|index| run.tools[index].narrowed_call().is_some());
            let primary = run.primary().is_some_and(|agent| agent.id == *agent_run_id);
            let tool = pending_tool(run, *agent_run_id)?;
            let narrowing_holds = !verdict.narrowed
                || primary
                    && !chained
                    && match tool.hooks.last().and_then(|hook| hook.outcome.as_ref()) {
                        Some(crate::tools::permissions::HookOutcome {
                            result: HookResult::Narrowed { call },
                            ..
                        }) => crate::tools::permissions::narrows(&tool.intent.call, call),
                        _ => false,
                    };
            if !narrowing_holds
                || tool.intent.digest() != verdict.intent_digest
                || tool.permission_verdict.is_some()
                || tool.observation.is_some()
            {
                return Err(ReplayError::InvalidTransition);
            }
            let exact_rule = verdict.basis
                == crate::tools::permissions::PermissionSource::ExactCommandRule
                || verdict.source == crate::tools::permissions::PermissionSource::ExactCommandRule;
            if exact_rule && !(rules_pinned && tool.intent.call.is_unguarded()) {
                return Err(ReplayError::InvalidTransition);
            }
            let hooks_open = tool.hooks.iter().any(|hook| hook.outcome.is_none());
            let folded = tool
                .hooks
                .iter()
                .filter_map(|hook| hook.outcome.as_ref())
                .fold(Decision::Allow, |folded, outcome| {
                    folded.narrow(outcome.result.restriction(&tool.intent.call))
                });
            let stopped = tool.hooks.iter().any(|hook| {
                hook.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.observation.stops_run())
            });
            let hooks_explained = match verdict.hook_restriction {
                None => folded == Decision::Allow && !stopped,
                Some(Decision::Ask) => folded == Decision::Ask && !stopped,
                Some(_) => folded == Decision::Deny || stopped || verdict.hooks == 0,
            };
            if hooks_open || usize::from(verdict.hooks) != tool.hooks.len() || !hooks_explained {
                return Err(ReplayError::InvalidTransition);
            }
            tool.permission_verdict = Some(verdict.clone());
        }
        Event::HookStarted {
            run_id,
            agent_run_id,
            hook,
        } => {
            if !matches!(hook.event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
                return start_lifecycle(current, *run_id, *agent_run_id, hook, prior);
            }
            let previous = current.previous_subject_sequence();
            let run = active_run(current, *run_id)?;
            if !permissions_pinned(run) || hook.intent.agent_run_id != *agent_run_id {
                return Err(ReplayError::InvalidTransition);
            }
            if hook.active_ceiling_ms.is_some() {
                return Err(ReplayError::InvalidTransition);
            }
            let post = hook.event == HookEvent::PostToolUse;
            let (tool, state_allows) = if post {
                let (tool, adjacent) =
                    post_target(run, *agent_run_id, &hook.subject_digest, previous)?;
                let succeeded = tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == crate::tools::ToolDisposition::Succeeded
                });
                (tool, adjacent && succeeded)
            } else {
                let tool = pending_tool(run, *agent_run_id)?;
                let fresh = tool.permission_verdict.is_none() && tool.observation.is_none();
                (tool, fresh)
            };
            let finished_restriction = tool.hooks.iter().any(|view| {
                view.outcome.as_ref().is_none_or(|outcome| {
                    outcome.result.restriction(&tool.intent.call) == Decision::Deny
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
                || tool.intent.agent_run_id != *agent_run_id
                || hook.subject_digest != tool.intent.digest()
                || hook.intent.policy_digest != tool.intent.policy_digest
                || hook.intent.workspace_device != tool.intent.workspace_device
                || hook.intent.workspace_inode != tool.intent.workspace_inode
                || hook.intent.id == tool.intent.id
                || same_event >= MAX_HOOKS_PER_OCCURRENCE
                || finished_restriction
                || tool
                    .hooks
                    .iter()
                    .any(|view| view.invocation.intent.id == hook.intent.id)
            {
                return Err(ReplayError::InvalidTransition);
            }
            tool.hooks.push(HookView {
                invocation: hook.clone(),
                outcome: None,
                idle: false,
            });
            if post {
                tool.hook_sequence = Some(prior);
            }
        }
        Event::HookFinished {
            run_id,
            agent_run_id,
            outcome,
        } => {
            if current
                .runs
                .iter()
                .find(|run| run.id == *run_id)
                .is_some_and(|run| {
                    run.lifecycle_hooks
                        .last()
                        .is_some_and(|hook| hook.outcome.is_none())
                })
            {
                let run = current
                    .runs
                    .iter_mut()
                    .find(|run| run.id == *run_id)
                    .ok_or(ReplayError::InvalidTransition)?;
                let hook = run
                    .lifecycle_hooks
                    .last_mut()
                    .ok_or(ReplayError::InvalidTransition)?;
                if hook.invocation.intent != outcome.observation.intent
                    || hook.invocation.intent.agent_run_id != *agent_run_id
                    || outcome.result
                        != HookResult::for_event(hook.invocation.event, &outcome.observation)
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
                            goal.status == crate::session::GoalStatus::Running
                                && goal.run_ids.last() == Some(run_id)
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
            let run = active_run(current, *run_id)?;
            if !permissions_pinned(run) {
                return Err(ReplayError::InvalidTransition);
            }
            if outcome.boundary_active_ms.is_some() {
                return Err(ReplayError::InvalidTransition);
            }
            let tool = if run.primary().is_some_and(|agent| agent.id == *agent_run_id) {
                run.tools
                    .iter_mut()
                    .rev()
                    .take(2)
                    .find(|tool| tool.hooks.last().is_some_and(|view| view.outcome.is_none()))
            } else {
                Some(pending_tool(run, *agent_run_id)?)
            }
            .ok_or(ReplayError::InvalidTransition)?;
            let observed = tool.observation.is_some();
            let sequence = tool.hook_sequence;
            let view = tool
                .hooks
                .last_mut()
                .filter(|view| {
                    view.outcome.is_none()
                        && outcome.observation.intent.agent_run_id == *agent_run_id
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
        }
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
            active_ceiling_ms,
        } if names_child(current, *run_id, *agent_run_id) => {
            current.start_child_tool(*run_id, *agent_run_id, intent)?;
            record_run_ceiling(current, *run_id, *active_ceiling_ms)?;
        }
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
            active_ceiling_ms,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let phase_allows_call =
                goal.phase == GoalPhase::Implementation || intent.call.is_goal_plan_prelude();
            let plan_allows_call = if matches!(
                intent.call,
                crate::tools::ToolCall::ParseAgentDefinition { .. }
            ) {
                true
            } else if let Some(contract) = &goal.plan_contract {
                !goal.awaiting_plan_contract
                    && (matches!(intent.call, crate::tools::ToolCall::PrepareWorker { .. })
                        || contract.allows_call(&intent.call)
                        || matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. }))
            } else {
                goal.awaiting_plan_contract && intent.call.is_goal_plan_prelude()
            };
            if !phase_allows_call
                || !plan_allows_call
                || goal
                    .effective_budget()
                    .max_tokens
                    .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let run = active_run(current, *run_id)?;
            let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
            let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
            let integration = matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. });
            let valid_origin =
                if let crate::tools::ToolCall::ParseAgentDefinition { index, raw_digest } =
                    &intent.call
                {
                    let parsed = run.tools.len();
                    primary.provider_calls.is_empty()
                        && run.agents.len() == 1
                        && usize::from(*index) == parsed
                        && config
                            .agent_definitions
                            .as_ref()
                            .and_then(|pin| pin.files.get(parsed))
                            .is_some_and(|file| file.digest == *raw_digest)
                        && run.tools.iter().all(|tool| {
                            tool.observation.as_ref().is_some_and(|observation| {
                                observation.disposition == crate::tools::ToolDisposition::Succeeded
                            })
                        })
                } else if let crate::tools::ToolCall::PrepareWorker { preparation } = &intent.call {
                    super::worker::base_preparation(run, preparation)
                } else if integration {
                    valid_integration_source(run, intent)
                } else {
                    primary.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                    }) && pending_tool_requests(run).is_some_and(|pending| pending >= 1)
                        && valid_narrowing_source(run, intent)
                        && run
                            .tools
                            .last()
                            .and_then(|tool| tool.observation.as_ref())
                            .is_none_or(|observation| !observation.stops_run())
                };
            if config
                .tool_policy
                .as_ref()
                .is_none_or(|policy| policy.config_digest != intent.policy_digest)
                || primary.id != *agent_run_id
                || primary.status != AgentStatus::Active
                || !valid_origin
                || !run.hooks_settled()
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
                checkpoint: None,
                intent: intent.clone(),
                observation: None,
                review_call_index: None,
                permission_verdict: None,
                hooks: Vec::new(),
                hook_sequence: None,
                finished_sequence: None,
            });
            record_run_ceiling(current, *run_id, *active_ceiling_ms)?;
        }
        Event::ToolFinished {
            checkpoint,
            run_id,
            agent_run_id,
            observation,
        } if names_child(current, *run_id, *agent_run_id) => {
            debug_assert!(checkpoint.is_none());
            current.finish_child_tool(*run_id, *agent_run_id, observation, prior)?;
        }
        Event::ToolFinished {
            checkpoint,
            run_id,
            agent_run_id,
            observation,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let out_of_plan_integration = matches!(
                &observation.intent.call,
                crate::tools::ToolCall::ApplyChangeset { .. }
            ) && !matches!(
                observation.intent.call,
                crate::tools::ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                }
            ) && goal
                .plan_contract
                .as_ref()
                .is_some_and(|contract| !contract.allows_call(&observation.intent.call));
            if out_of_plan_integration
                && (observation.disposition != crate::tools::ToolDisposition::Denied
                    || observation.attestation.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let timeout_limit = match &observation.intent.call {
                crate::tools::ToolCall::Command { .. } => {
                    let run = current
                        .runs
                        .iter()
                        .find(|run| run.id == *run_id)
                        .ok_or(ReplayError::InvalidTransition)?;
                    let contract_timeout = goal
                        .plan_contract
                        .as_ref()
                        .and_then(|contract| contract.command_timeout_ms);
                    let requested = contract_timeout
                        .unwrap_or(goal.resource_settings.default_command_timeout_ms);
                    (requested > goal.resource_settings.max_command_timeout_ms)
                        .then_some((requested, goal.resource_settings.max_command_timeout_ms))
                        .map(|limit| {
                            (
                                limit,
                                run.tools
                                    .last()
                                    .is_some_and(|tool| tool.review_call_index.is_none()),
                            )
                        })
                }
                _ => None,
            };
            if let Some(((requested, maximum), no_review)) = timeout_limit
                && (!no_review
                    || observation.intent.limits.runtime_ms != requested
                    || observation.disposition != crate::tools::ToolDisposition::Limit
                    || observation.attestation.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
                    || observation.output
                        != crate::tools::types::command_timeout_limit_message(requested, maximum))
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
                tool.checkpoint = checkpoint.clone();
                tool.observation = Some(observation.clone());
                tool.finished_sequence = Some(prior);
                if matches!(
                    observation.intent.call,
                    crate::tools::ToolCall::ParseAgentDefinition { .. }
                ) {
                    run.definitions = super::DefinitionCache::default();
                    let computed = super::compute_definition_registry(run).ok();
                    run.definitions = super::DefinitionCache(computed);
                }
            }
            let goal_id = current
                .goals
                .iter()
                .find(|goal| goal.active_run == Some(*run_id))
                .map(|goal| goal.id)
                .ok_or(ReplayError::InvalidTransition)?;
            let goal = current_goal_mut(current, goal_id)?;
            if let Some(contract) = goal.plan_contract.as_ref() {
                if matches!(
                    observation.intent.call,
                    crate::tools::ToolCall::ApplyChangeset {
                        source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                    }
                ) {
                    goal.success_state = contract.initial_success_state();
                } else {
                    contract.observe_success(&mut goal.success_state, observation);
                }
                goal.success_criterion_satisfied =
                    contract.success_state_satisfied(&goal.success_state);
            }
            // Tools run one at a time, so once every declared request has started, this observation completes the step.
            let all_requests_observed = current
                .runs
                .iter()
                .find(|run| run.id == *run_id)
                .and_then(pending_tool_requests)
                == Some(0);
            let unplanned_prelude = current
                .goal(goal_id)
                .is_some_and(|goal| goal.plan_contract.is_none())
                && observation.intent.call.is_goal_plan_prelude();
            if unplanned_prelude && all_requests_observed {
                current_goal_mut(current, goal_id)?.awaiting_plan_contract = false;
            }
        }
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
    let tail = integration
        .finished_sequence
        .max(tools[index].hook_sequence);
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
                        (Some(ChildGrant::ReadOnly), false) | (Some(ChildGrant::EditOnly), true)
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
