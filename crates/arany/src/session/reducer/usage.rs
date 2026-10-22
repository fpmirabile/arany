use super::{AgentRunId, Event, ReplayError, RunId, SessionId, SessionView};
use crate::provider::{ProviderRole, Usage};

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageView {
    pub groups: Vec<UsageGroup>,
    pub pending: Vec<(SessionId, RunId, Option<AgentRunId>)>,
}

pub(super) fn apply(view: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
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
