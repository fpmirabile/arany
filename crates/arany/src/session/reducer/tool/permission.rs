use super::{SessionView, active_run, tool_by_digest, valid_host_approval};
use crate::provider::AgentPhase;
use crate::session::events::{Event, ProviderCallDisposition, ReplayError};
use crate::tools::permissions::{
    Decision, HookResult, PermissionMode, PermissionSource, TypedGitPrefix,
};

pub(super) fn validate_modes(
    call: &crate::tools::ToolCall,
    verdict: &crate::tools::permissions::PermissionVerdict,
    restrictions: Vec<PermissionMode>,
) -> Result<(), ReplayError> {
    for mode in restrictions {
        let restriction = crate::tools::permissions::PermissionConfig::evaluate_mode(mode, call).0;
        if verdict.approved
            && !(call.is_unguarded()
                && mode != PermissionMode::Plan
                && matches!(
                    verdict.source,
                    PermissionSource::SessionApproval | PermissionSource::HostPrefixRule
                ))
            && (restriction == crate::tools::permissions::Decision::Deny
                || restriction == crate::tools::permissions::Decision::Ask
                    && !matches!(
                        verdict.source,
                        PermissionSource::Human | PermissionSource::AiReview
                    ))
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

fn has_new_reads(run: &crate::session::reducer::RunView, call: &crate::tools::ToolCall) -> bool {
    let crate::tools::ToolCall::Command {
        additional_reads: Some(request),
        ..
    } = call
    else {
        return false;
    };
    let saved = run
        .config
        .as_ref()
        .and_then(|config| config.tool_policy.as_ref())
        .map_or(&[][..], |policy| policy.guard.saved_reads.as_slice());
    request.paths.iter().any(|path| {
        !saved.iter().any(|read| read.path == *path)
            && !run.tools.iter().any(|tool| {
                tool.permission_verdict.as_ref().is_some_and(|verdict| {
                    verdict.approved && verdict.source == PermissionSource::Human
                }) && matches!(
                    &tool.intent.call,
                    crate::tools::ToolCall::Command { additional_reads: Some(earlier), .. }
                        if earlier.paths.contains(path)
                )
            })
    })
}

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::ToolStarted {
        run_id,
        agent_run_id,
        verdict: Some(verdict),
        ..
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let host_valid = valid_host_approval(current, *run_id, *agent_run_id, verdict);
    let run = active_run(current, *run_id)?;
    let facts = VerdictFacts::new(run, agent_run_id, verdict, host_valid)?;
    facts.check_first_use(run)?;
    facts.check_rules()?;
    facts.check_host(run)?;
    facts.check_review(run)?;
    facts.record(run)?;
    if facts.unguarded && verdict.approved {
        for agent in &mut run.agents {
            if let Some(worker) = &mut agent.assignment {
                worker.verification = None;
            }
        }
        let goal = current
            .goals
            .iter_mut()
            .find(|goal| goal.active_run == Some(*run_id))
            .ok_or(ReplayError::InvalidTransition)?;
        if let Some(contract) = &goal.plan_contract {
            goal.success_state = contract.initial_success_state();
            goal.success_criterion_satisfied = false;
        }
    }
    Ok(())
}

struct VerdictFacts<'a> {
    verdict: &'a crate::tools::permissions::PermissionVerdict,
    agent_run_id: &'a crate::session::AgentRunId,
    pending_call: crate::tools::ToolCall,
    primary: bool,
    unguarded: bool,
    permissions_pinned: bool,
    hooks_pinned: bool,
    definition_mode: Option<PermissionMode>,
    first_use: bool,
    first_unconfigured_host_read: bool,
    host_valid: bool,
    repository: Option<String>,
}

impl<'a> VerdictFacts<'a> {
    fn new(
        run: &mut crate::session::reducer::RunView,
        agent_run_id: &'a crate::session::AgentRunId,
        verdict: &'a crate::tools::permissions::PermissionVerdict,
        host_valid: bool,
    ) -> Result<Self, ReplayError> {
        let definition_mode = super::definition_mode(run, *agent_run_id)?;
        let policy = run
            .config
            .as_ref()
            .and_then(|config| config.tool_policy.as_ref());
        let permissions_pinned = policy.is_some_and(|policy| policy.permissions_digest.is_some());
        let hooks_pinned = policy
            .and_then(|policy| policy.git.as_ref())
            .is_some_and(|git| git.hooks.is_some());
        let repository = policy
            .and_then(|policy| policy.git.as_ref())
            .and_then(|git| git.repository.clone());
        let pending_call = tool_by_digest(run, *agent_run_id, &verdict.intent_digest)?
            .intent
            .call
            .clone();
        validate_modes(
            &pending_call,
            verdict,
            super::super::mode_restrictions(run, *agent_run_id)?,
        )?;
        let unguarded = pending_call.is_unguarded();
        let primary = run.primary().is_some_and(|agent| agent.id == *agent_run_id);
        let first_use = !permissions_pinned
            && definition_mode.is_none()
            && !unguarded
            && pending_call.is_git()
            && pending_call.facts().mutates;
        let first_unconfigured_host_read =
            !permissions_pinned && definition_mode.is_none() && pending_call.is_host_pr_read();
        Ok(Self {
            verdict,
            agent_run_id,
            pending_call,
            primary,
            unguarded,
            permissions_pinned,
            hooks_pinned,
            definition_mode,
            first_use,
            first_unconfigured_host_read,
            host_valid,
            repository,
        })
    }

    fn check_first_use(&self, run: &crate::session::reducer::RunView) -> Result<(), ReplayError> {
        let &Self {
            verdict,
            ref pending_call,
            primary,
            hooks_pinned,
            first_use,
            ..
        } = self;
        if verdict.approved
            && has_new_reads(run, pending_call)
            && !(primary && verdict.source == PermissionSource::Human)
        {
            return Err(ReplayError::InvalidTransition);
        }
        if first_use
            && !(primary
                && verdict.hook_restriction.is_none()
                && matches!(
                    verdict.basis,
                    PermissionSource::Mode | PermissionSource::PinnedGrant
                )
                && matches!(
                    verdict.source,
                    PermissionSource::Mode
                        | PermissionSource::PinnedGrant
                        | PermissionSource::Human
                        | PermissionSource::AiReview
                        | PermissionSource::UnavailableApproval
                ))
        {
            return Err(ReplayError::InvalidTransition);
        }
        // With accepted hooks a Git write is approved by a human, a user-owned allow rule or auto's review.
        if verdict.approved
            && pending_call.facts().site == crate::tools::catalog::Site::Git
            && pending_call.mutates()
            && hooks_pinned
            && !matches!(
                verdict.source,
                PermissionSource::Human
                    | PermissionSource::SessionApproval
                    | PermissionSource::Rule
                    | PermissionSource::TypedPrefixRule
                    | PermissionSource::AiReview
            )
        {
            return Err(ReplayError::InvalidTransition);
        }
        Ok(())
    }

    fn check_rules(&self) -> Result<(), ReplayError> {
        let &Self {
            verdict,
            ref pending_call,
            primary,
            unguarded,
            permissions_pinned,
            definition_mode,
            first_use,
            first_unconfigured_host_read,
            host_valid,
            ..
        } = self;
        let typed_basis = verdict.basis == PermissionSource::TypedPrefixRule;
        let typed_source = verdict.source == PermissionSource::TypedPrefixRule;
        if (typed_basis || typed_source)
            && !(permissions_pinned
                && primary
                && typed_basis
                && TypedGitPrefix::of_call(pending_call).is_some()
                && (!typed_source
                    // An allow whose dispatch a later plan, budget or pause gate refused is unapproved.
                    || verdict.decision == Decision::Allow
                        && (matches!(
                            verdict.mode,
                            Some(
                                PermissionMode::Default
                                    | PermissionMode::DontAsk
                                    | PermissionMode::AcceptEdits
                                    | PermissionMode::Auto
                            )
                        ) || pending_call.is_host_pr_read())
                    || !verdict.approved && verdict.decision == Decision::Deny))
        {
            return Err(ReplayError::InvalidTransition);
        }
        if (verdict.basis == PermissionSource::HostPrefixRule
            || verdict.source == PermissionSource::HostPrefixRule)
            && !(permissions_pinned
                && primary
                && unguarded
                && verdict.basis == PermissionSource::HostPrefixRule
                && (!verdict.approved || host_valid))
        {
            return Err(ReplayError::InvalidTransition);
        }
        if verdict.mode.is_some() != (permissions_pinned || definition_mode.is_some())
            || !(permissions_pinned
                || definition_mode.is_some()
                || unguarded
                || first_use
                || first_unconfigured_host_read)
            || definition_mode == Some(crate::tools::permissions::PermissionMode::DontAsk)
                && verdict.decision == crate::tools::permissions::Decision::Ask
        {
            return Err(ReplayError::InvalidTransition);
        }
        Ok(())
    }

    fn check_host(&self, run: &mut crate::session::reducer::RunView) -> Result<(), ReplayError> {
        let &Self {
            verdict,
            agent_run_id,
            ref pending_call,
            primary,
            unguarded,
            permissions_pinned,
            host_valid,
            ref repository,
            ..
        } = self;
        if pending_call.is_host()
            && verdict.approved
            && (!primary
                || tool_by_digest(run, *agent_run_id, &verdict.intent_digest)?
                    .intent
                    .host
                    .is_none()
                || if pending_call.is_host_pr_read() {
                    !(verdict.source == PermissionSource::TypedPrefixRule
                        && verdict.basis == PermissionSource::TypedPrefixRule
                        && verdict.decision == Decision::Allow
                        && permissions_pinned)
                        && (verdict.source != PermissionSource::Human
                            || verdict.decision != Decision::Ask
                            || matches!(verdict.mode, Some(PermissionMode::DontAsk)))
                } else {
                    let human = verdict.source == PermissionSource::Human
                        && verdict.decision == Decision::Ask
                        && !matches!(
                            verdict.mode,
                            Some(PermissionMode::Plan | PermissionMode::DontAsk)
                        );
                    // A user's typed allow covers any push or PR; auto's review only the pinned repository.
                    let same_repository = {
                        let intent =
                            &tool_by_digest(run, *agent_run_id, &verdict.intent_digest)?.intent;
                        repository.as_deref().is_some_and(|pin| {
                            crate::tools::same_repository(pin, &intent.call, intent.host.as_ref())
                        })
                    };
                    let delegated = same_repository
                        && verdict.source == PermissionSource::AiReview
                        && verdict.decision == Decision::Ask
                        && matches!(verdict.mode, None | Some(PermissionMode::Auto))
                        || verdict.source == PermissionSource::TypedPrefixRule
                            && verdict.basis == PermissionSource::TypedPrefixRule
                            && verdict.decision == Decision::Allow
                            && permissions_pinned;
                    !human && !delegated
                })
        {
            return Err(ReplayError::InvalidTransition);
        }
        if unguarded && verdict.approved && !host_valid {
            return Err(ReplayError::InvalidTransition);
        }
        if !unguarded && verdict.host.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
        Ok(())
    }

    fn check_review(&self, run: &mut crate::session::reducer::RunView) -> Result<(), ReplayError> {
        let &Self {
            verdict,
            agent_run_id,
            ..
        } = self;
        if verdict.source == crate::tools::permissions::PermissionSource::AiReview
            && verdict.approved
        {
            let index = tool_by_digest(run, *agent_run_id, &verdict.intent_digest)?
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
        Ok(())
    }

    fn record(&self, run: &mut crate::session::reducer::RunView) -> Result<(), ReplayError> {
        let &Self {
            verdict,
            agent_run_id,
            primary,
            unguarded,
            host_valid,
            ..
        } = self;
        let chained = run
            .tools
            .iter()
            .position(|tool| {
                tool.observation.is_none() && tool.intent.digest() == verdict.intent_digest
            })
            .and_then(|index| index.checked_sub(1))
            .is_some_and(|index| run.tools[index].narrowed_call().is_some());
        let tool = tool_by_digest(run, *agent_run_id, &verdict.intent_digest)?;
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
        if (verdict.basis == PermissionSource::SessionApproval
            || verdict.source == PermissionSource::SessionApproval)
            && !(unguarded && host_valid)
        {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests;
