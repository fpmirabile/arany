use super::{AgentRunId, Event, ReplayError, RunId, SessionId, SessionView};
use crate::provider::{
    ApiLimit, ApiLimitDialect, LimitFailure, ProviderIdentity, ProviderKind, ProviderRole,
    RateLimits,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiUsage {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub actor: Option<AgentRunId>,
    pub role: ProviderRole,
    pub identity: ProviderIdentity,
    pub dialect: ApiLimitDialect,
    pub rows: Vec<ApiLimit>,
    pub latest_unavailable: Option<LimitFailure>,
    pub latest_unreported: bool,
}

pub(super) fn apply(
    view: &mut SessionView,
    event: &Event,
    scope: (RunId, Option<AgentRunId>, ProviderRole),
    limits: Option<&RateLimits>,
) -> Result<(), ReplayError> {
    let (run_id, actor, role) = scope;
    let sample = limits.and_then(|limits| limits.api.as_ref());
    if sample.is_none()
        && !view.usage.api_limits.iter().any(|group| {
            group.session_id == view.id
                && group.run_id == run_id
                && group.actor == actor
                && group.role == role
        })
    {
        return Ok(());
    }
    let identity = identity(view, event, scope).ok_or(ReplayError::InvalidTransition)?;
    if let Some(sample) = sample {
        let admitted = match ProviderKind::parse(&identity.profile) {
            Some(ProviderKind::OpenAi) => sample.dialect == ApiLimitDialect::OpenAi,
            Some(ProviderKind::Anthropic) => sample.dialect == ApiLimitDialect::Anthropic,
            Some(ProviderKind::Custom) => identity.custom_provenance().is_some(),
            _ => false,
        };
        if !admitted {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let index = view.usage.api_limits.iter().position(|group| {
        group.session_id == view.id
            && group.run_id == run_id
            && group.actor == actor
            && group.role == role
            && group.identity == identity
    });
    let index = match index {
        Some(index) => index,
        None => {
            let Some(sample) = sample else { return Ok(()) };
            if view.usage.api_limits.len() >= crate::store::MAX_SESSION_EVENTS {
                return Err(ReplayError::InvalidTransition);
            }
            view.usage.api_limits.push(ApiUsage {
                session_id: view.id,
                run_id,
                actor,
                role,
                identity,
                dialect: sample.dialect,
                rows: Vec::new(),
                latest_unavailable: None,
                latest_unreported: true,
            });
            view.usage.api_limits.len() - 1
        }
    };
    update(&mut view.usage.api_limits[index], sample)
}

fn update(
    group: &mut ApiUsage,
    sample: Option<&crate::provider::ApiLimits>,
) -> Result<(), ReplayError> {
    group.latest_unreported = sample.is_none_or(|sample| {
        group
            .rows
            .iter()
            .any(|old| !sample.rows.iter().any(|row| row.dimension == old.dimension))
    });
    group.latest_unavailable = sample.and_then(|sample| sample.unavailable);
    let Some(sample) = sample else { return Ok(()) };
    if group.dialect != sample.dialect {
        return Err(ReplayError::InvalidTransition);
    }
    for row in &sample.rows {
        if let Some(reason) = row.unavailable {
            group.latest_unavailable = Some(LimitFailure {
                reason,
                source: row.source,
                observed_at: row.observed_at,
            });
        }
        if let Some(old) = group
            .rows
            .iter_mut()
            .find(|old| old.dimension == row.dimension)
        {
            if row.unavailable.is_none()
                && (old.unavailable.is_some() || row.observed_at >= old.observed_at)
            {
                *old = row.clone();
            }
        } else {
            group.rows.push(row.clone());
        }
    }
    Ok(())
}

fn identity(
    view: &SessionView,
    event: &Event,
    (run_id, actor, role): (RunId, Option<AgentRunId>, ProviderRole),
) -> Option<ProviderIdentity> {
    if let Event::ProviderFallbackUsed { fallback, .. } = event {
        return Some(fallback.from.clone());
    }
    let run = view.runs.iter().find(|run| run.id == run_id)?;
    let config = run.config.as_ref()?;
    if let Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } =
        event
    {
        return config
            .providers
            .as_ref()
            .and_then(|routes| {
                routes
                    .compaction
                    .iter()
                    .find(|identity| {
                        identity.profile == record.provider && identity.model == record.model
                    })
                    .cloned()
            })
            .or_else(|| {
                config.identity().filter(|identity| {
                    identity.profile == record.provider && identity.model == record.model
                })
            });
    }
    let actor = actor?;
    run.provider_fallbacks
        .iter()
        .rev()
        .find(|(id, fallback)| *id == actor && fallback.role == role)
        .map(|(_, fallback)| fallback.to.clone())
        .or_else(|| {
            run.provider_chain(actor, role)
                .and_then(|chain| chain.first())
                .cloned()
        })
        .or_else(|| config.identity())
}
