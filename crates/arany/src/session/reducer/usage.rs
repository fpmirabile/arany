use super::{AgentRunId, Event, ReplayError, RunId, SessionId, SessionView};
use crate::provider::{ProviderKind, ProviderRole, Usage};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenCount {
    pub known: Option<u64>,
    pub unreported: u64,
}

impl TokenCount {
    fn add(&mut self, value: Option<u32>) -> Result<(), ReplayError> {
        match value {
            Some(value) => {
                self.known = Some(
                    self.known
                        .unwrap_or(0)
                        .checked_add(u64::from(value))
                        .ok_or(ReplayError::InvalidTransition)?,
                );
            }
            None => {
                self.unreported = self
                    .unreported
                    .checked_add(1)
                    .ok_or(ReplayError::InvalidTransition)?
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageGroup {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub provider: String,
    pub model: String,
    pub input: TokenCount,
    pub output: TokenCount,
    pub cache_read: TokenCount,
    pub cache_creation: TokenCount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanUsage {
    account: Option<uuid::Uuid>,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub provider: String,
    pub model: String,
    pub primary: Option<crate::provider::PlanWindow>,
    pub secondary: Option<crate::provider::PlanWindow>,
    pub latest_unavailable: Option<crate::provider::LimitFailure>,
    pub latest_unreported: bool,
}

impl PlanUsage {
    #[must_use]
    pub fn matches_account(&self, account: uuid::Uuid) -> bool {
        self.account == Some(account)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageView {
    pub plan_limits: Vec<PlanUsage>,
    pub groups: Vec<UsageGroup>,
    pub pending: Vec<(SessionId, RunId, Option<AgentRunId>)>,
}

pub(super) fn apply(view: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let limits = match event {
        Event::ProviderCallRecorded { record, .. } => record.rate_limits.as_ref(),
        Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => {
            record.rate_limits.as_ref()
        }
        Event::ProviderFallbackUsed { fallback, .. } => fallback.rate_limits.as_ref(),
        Event::ProviderRetryScheduled { retry, .. } => retry.rate_limits.as_ref(),
        _ => None,
    };
    if limits.is_some_and(|limits| !limits.valid()) {
        return Err(ReplayError::InvalidTransition);
    }
    let (run_id, actor, role, usage, route, settle) = match event {
        Event::GoalProviderCallReserved {
            run_id,
            agent_run_id,
            ..
        } => {
            if view.usage.pending.len() >= 10_000 {
                return Err(ReplayError::InvalidTransition);
            }
            view.usage
                .pending
                .push((view.id, *run_id, Some(*agent_run_id)));
            return Ok(());
        }
        Event::GoalCompactionCallReserved { run_id, .. } => {
            if view.usage.pending.len() >= 10_000 {
                return Err(ReplayError::InvalidTransition);
            }
            view.usage.pending.push((view.id, *run_id, None));
            return Ok(());
        }
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record,
        } => (
            *run_id,
            Some(*agent_run_id),
            ProviderRole::for_phase(record.phase),
            Usage {
                input_tokens: record.input_tokens,
                output_tokens: record.output_tokens,
                cache_read_input_tokens: record.cache_read_input_tokens,
                cache_creation_input_tokens: record.cache_creation_input_tokens,
            },
            None,
            true,
        ),
        Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => (
            record.covered_run_id,
            None,
            ProviderRole::Compaction,
            Usage {
                input_tokens: record.input_tokens,
                output_tokens: record.output_tokens,
                cache_read_input_tokens: record.cache_read_input_tokens,
                cache_creation_input_tokens: record.cache_creation_input_tokens,
            },
            Some((record.provider.clone(), record.model.clone())),
            true,
        ),
        Event::ProviderFallbackUsed {
            run_id,
            agent_run_id,
            fallback,
        } => (
            *run_id,
            Some(*agent_run_id),
            fallback.role,
            Usage::default(),
            Some((fallback.from.profile.clone(), fallback.from.model.clone())),
            false,
        ),
        Event::ProviderRetryScheduled {
            run_id,
            agent_run_id,
            retry,
        } => (
            *run_id,
            Some(*agent_run_id),
            retry.role,
            Usage::default(),
            None,
            false,
        ),
        _ => return Ok(()),
    };
    let (provider, model) = match route {
        Some(route) => route,
        None => {
            let run = view
                .runs
                .iter()
                .find(|run| run.id == run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let actor = actor.ok_or(ReplayError::InvalidTransition)?;
            let fallback = run
                .provider_fallbacks
                .iter()
                .rev()
                .find(|(id, fallback)| *id == actor && fallback.role == role)
                .map(|(_, fallback)| &fallback.to);
            let initial = run
                .provider_chain(actor, role)
                .and_then(|chain| chain.first());
            if let Some(identity) = fallback.or(initial) {
                (identity.profile.clone(), identity.model.clone())
            } else {
                let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
                (config.provider.clone(), config.model.clone())
            }
        }
    };
    let chatgpt = ProviderKind::parse(&provider) == Some(ProviderKind::ChatGpt);
    if limits.is_some() && !chatgpt {
        return Err(ReplayError::InvalidTransition);
    }
    if chatgpt {
        let run = view
            .runs
            .iter()
            .find(|run| run.id == run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        let account = match event {
            Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => {
                record.chatgpt_provenance.as_ref().map(|p| p.account_id)
            }
            Event::ProviderFallbackUsed { fallback, .. } => {
                fallback.from.chatgpt_provenance().map(|p| p.account_id)
            }
            _ => {
                let actor = actor.ok_or(ReplayError::InvalidTransition)?;
                let identity = run
                    .provider_fallbacks
                    .iter()
                    .rev()
                    .find(|(id, fallback)| *id == actor && fallback.role == role)
                    .map(|(_, f)| &f.to)
                    .or_else(|| {
                        run.provider_chain(actor, role)
                            .and_then(|chain| chain.first())
                    });
                identity
                    .and_then(|i| i.chatgpt_provenance())
                    .or_else(|| {
                        run.config
                            .as_ref()
                            .and_then(|c| c.chatgpt_provenance.as_ref())
                    })
                    .map(|p| p.account_id)
            }
        };
        if limits.is_some() && account.is_none() {
            return Err(ReplayError::InvalidTransition);
        }
        let index = view.usage.plan_limits.iter().position(|group| {
            group.account == account
                && group.session_id == view.id
                && group.run_id == run_id
                && group.provider == provider
                && group.model == model
        });
        let index = match index {
            Some(index) => index,
            None => {
                if view.usage.plan_limits.len() >= 10_000 {
                    return Err(ReplayError::InvalidTransition);
                }
                view.usage.plan_limits.push(PlanUsage {
                    account,
                    session_id: view.id,
                    run_id,
                    provider: provider.clone(),
                    model: model.clone(),
                    primary: None,
                    secondary: None,
                    latest_unavailable: None,
                    latest_unreported: true,
                });
                view.usage.plan_limits.len() - 1
            }
        };
        let group = &mut view.usage.plan_limits[index];
        group.latest_unreported = limits.is_none();
        group.latest_unavailable = limits.and_then(|limits| limits.unavailable);
        if let Some(limits) = limits {
            for (old, new) in [
                (&mut group.primary, &limits.primary),
                (&mut group.secondary, &limits.secondary),
            ] {
                if let Some(new) = new
                    && old
                        .as_ref()
                        .is_none_or(|old| new.observed_at >= old.observed_at)
                {
                    *old = Some(new.clone());
                }
            }
        }
    }
    if settle
        && let Some(index) = view
            .usage
            .pending
            .iter()
            .rposition(|pending| *pending == (view.id, run_id, actor))
    {
        view.usage.pending.remove(index);
    }
    let groups = &mut view.usage.groups;
    let index = groups.iter().position(|group| {
        group.session_id == view.id
            && group.run_id == run_id
            && group.provider == provider
            && group.model == model
    });
    let index = match index {
        Some(index) => index,
        None => {
            // A group needs a canonical attempt within the 10,000-Event lineage ceiling.
            if groups.len() >= 10_000 {
                return Err(ReplayError::InvalidTransition);
            }
            groups.push(UsageGroup {
                session_id: view.id,
                run_id,
                provider,
                model,
                input: TokenCount::default(),
                output: TokenCount::default(),
                cache_read: TokenCount::default(),
                cache_creation: TokenCount::default(),
            });
            groups.len() - 1
        }
    };
    let group = &mut groups[index];
    group.input.add(usage.input_tokens)?;
    group.output.add(usage.output_tokens)?;
    group.cache_read.add(usage.cache_read_input_tokens)?;
    group
        .cache_creation
        .add(usage.cache_creation_input_tokens)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_usage_matches_only_its_pinned_account() {
        let account = uuid::Uuid::now_v7();
        let usage = PlanUsage {
            account: Some(account),
            session_id: SessionId::new(),
            run_id: RunId::new(),
            provider: "chatgpt".into(),
            model: "model-a".into(),
            primary: None,
            secondary: None,
            latest_unavailable: None,
            latest_unreported: true,
        };

        assert!(usage.matches_account(account));
        assert!(!usage.matches_account(uuid::Uuid::now_v7()));
    }
}
