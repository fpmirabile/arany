use super::{AgentPhase, Provenance, Provider, ProviderIdentity};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_PROVIDER_FALLBACKS: usize = 2;
pub const MAX_PROVIDER_SWITCHES: usize = 64;
pub const MAX_PROVIDER_RETRIES: u8 = 3;
// The longest pinned role deadline bounds every wait, including a server Retry-After.
pub const MAX_PROVIDER_RETRY_DELAY_MS: u32 = 20 * 60 * 1000;
const RETRY_BASE: Duration = Duration::from_secs(2);
const RETRY_GAP: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    Primary,
    Child,
    Review,
    Compaction,
}

impl ProviderRole {
    fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Child => "child",
            Self::Review => "review",
            Self::Compaction => "compaction",
        }
    }

    pub(crate) fn for_phase(phase: AgentPhase) -> Self {
        match phase {
            AgentPhase::RootPlan | AgentPhase::RootSynthesis => Self::Primary,
            AgentPhase::ChildWork => Self::Child,
            AgentPhase::ToolReview => Self::Review,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
pub struct Providers<T> {
    #[serde(deserialize_with = "T::deserialize")]
    pub primary: T,
    #[serde(deserialize_with = "T::deserialize")]
    pub child: T,
    #[serde(deserialize_with = "T::deserialize")]
    pub review: T,
    #[serde(deserialize_with = "T::deserialize")]
    pub compaction: T,
}

impl<T> Providers<T> {
    pub fn get(&self, role: ProviderRole) -> &T {
        match role {
            ProviderRole::Primary => &self.primary,
            ProviderRole::Child => &self.child,
            ProviderRole::Review => &self.review,
            ProviderRole::Compaction => &self.compaction,
        }
    }

    pub fn get_mut(&mut self, role: ProviderRole) -> &mut T {
        match role {
            ProviderRole::Primary => &mut self.primary,
            ProviderRole::Child => &mut self.child,
            ProviderRole::Review => &mut self.review,
            ProviderRole::Compaction => &mut self.compaction,
        }
    }

    pub fn values(&self) -> [&T; 4] {
        [&self.primary, &self.child, &self.review, &self.compaction]
    }
}

impl<T: Clone> Providers<T> {
    pub fn all(primary: T) -> Self {
        Self {
            child: primary.clone(),
            review: primary.clone(),
            compaction: primary.clone(),
            primary,
        }
    }
}

pub type ProviderRoutes = Providers<Vec<ProviderIdentity>>;
#[derive(Clone)]
pub struct AdmittedProviders {
    pub roles: Providers<Vec<Arc<dyn Provider>>>,
    pub roster: Option<super::AdmittedRoster>,
}

impl std::ops::Deref for AdmittedProviders {
    type Target = Providers<Vec<Arc<dyn Provider>>>;

    fn deref(&self) -> &Self::Target {
        &self.roles
    }
}

impl AdmittedProviders {
    pub fn single(provider: Arc<dyn Provider>) -> Self {
        Self {
            roles: Providers::all(vec![provider]),
            roster: None,
        }
    }

    pub fn from_roles(roles: Providers<Vec<Arc<dyn Provider>>>) -> Self {
        Self {
            roles,
            roster: None,
        }
    }

    pub(crate) fn child_chains(&self) -> Vec<&Vec<Arc<dyn Provider>>> {
        let mut chains = vec![&self.roles.child];
        if let Some(roster) = &self.roster {
            for name in &roster.snapshot.child_profiles {
                if let Some((_, chain)) = roster
                    .profiles
                    .iter()
                    .find(|(candidate, _)| candidate == name)
                {
                    chains.push(chain);
                }
            }
        }
        chains
    }

    pub(crate) fn agent_chains(&self) -> Vec<&Vec<Arc<dyn Provider>>> {
        let mut chains = vec![&self.roles.primary];
        chains.extend(self.child_chains());
        chains
    }

    pub(crate) fn agent_concurrency(&self) -> u8 {
        self.agent_chains()
            .into_iter()
            .flatten()
            .map(|provider| provider.identity().limits.concurrency)
            .min()
            .unwrap_or(1)
    }

    // Every primary route must carry a window, so a fallback never receives a request above its own ceiling.
    pub(crate) async fn context_window_tokens_for_run(&self) -> Option<u32> {
        let mut window = None;
        for provider in &self.primary {
            let current = provider.context_window_tokens_for_run().await?;
            window = Some(window.map_or(current, |window: u32| window.min(current)));
        }
        window
    }

    pub(crate) fn identities(&self) -> ProviderRoutes {
        Providers {
            primary: identities(&self.primary),
            child: identities(&self.child),
            review: identities(&self.review),
            compaction: identities(&self.compaction),
        }
    }
}

fn identities(chain: &[Arc<dyn Provider>]) -> Vec<ProviderIdentity> {
    chain
        .iter()
        .map(|provider| provider.identity().clone())
        .collect()
}

impl ProviderRoutes {
    pub(crate) fn valid(&self) -> bool {
        self.values().iter().all(|chain| {
            !chain.is_empty()
                && chain.len() <= MAX_PROVIDER_FALLBACKS + 1
                && chain.iter().all(|identity| {
                    identity.valid()
                        && !identity.profile.chars().any(char::is_control)
                        && !identity.model.chars().any(char::is_control)
                        && !identity.limits.deadline.is_zero()
                        && identity.limits.deadline <= std::time::Duration::from_secs(1200)
                })
                && chain
                    .iter()
                    .enumerate()
                    .all(|(index, identity)| !chain[..index].iter().any(|prior| prior == identity))
        })
    }
}

impl ProviderIdentity {
    // ChatGPT evidence may differ per model within one account; other provenance must match exactly.
    pub(crate) fn shares_account(&self, other: &Self) -> bool {
        self.profile == other.profile
            && match (&self.provenance, &other.provenance) {
                (Provenance::ChatGpt(first), Provenance::ChatGpt(second)) => {
                    first.account_id == second.account_id
                }
                (first, second) => first == second,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatGptAdmission, ChatGptProvenance};
    use uuid::Uuid;

    fn chatgpt(model: &str, account: u128, fingerprint: u8) -> ProviderIdentity {
        ProviderIdentity::new(
            "chatgpt",
            model,
            Provenance::ChatGpt(ChatGptProvenance {
                account_id: Uuid::from_u128(account),
                evidence_fingerprint: [fingerprint; 32],
                admission: ChatGptAdmission::AccountConsent,
            }),
        )
    }

    #[test]
    fn one_chatgpt_account_shares_across_models_and_evidence() {
        assert!(chatgpt("model-a", 1, 1).shares_account(&chatgpt("model-b", 1, 2)));
    }

    #[test]
    fn different_chatgpt_accounts_never_share() {
        assert!(!chatgpt("model-a", 1, 1).shares_account(&chatgpt("model-a", 2, 1)));
    }

    #[test]
    fn chatgpt_and_api_key_identities_never_share() {
        let api_key = ProviderIdentity::new("chatgpt", "model-a", Provenance::native());
        assert!(!chatgpt("model-a", 1, 1).shares_account(&api_key));
        assert!(!api_key.shares_account(&chatgpt("model-a", 1, 1)));
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFallback {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<crate::provider::RateLimits>,
    pub role: ProviderRole,
    pub call_index: u32,
    pub failure: crate::session::ProviderCallDisposition,
    pub from: ProviderIdentity,
    pub to: ProviderIdentity,
}

impl ProviderFallback {
    pub fn notice(&self) -> String {
        let role = self.role.label();
        format!(
            "Provider fallback ({role}): the failed call may have consumed usage. {}/{} to {}/{}.",
            self.from.profile, self.from.model, self.to.profile, self.to.model
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryCause {
    Transport,
    RateLimited,
    ServerError,
    StreamCut,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRetry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<crate::provider::RateLimits>,
    pub role: ProviderRole,
    pub call_index: u32,
    pub attempt: u8,
    pub cause: RetryCause,
    pub delay_ms: u32,
}

impl ProviderRetry {
    /// Exponential gap before same-route `attempt` (one-based): 2 s doubling, at most 10 s.
    pub(crate) fn backoff(attempt: u8) -> Duration {
        RETRY_BASE
            .saturating_mul(1 << attempt.saturating_sub(1).min(8))
            .min(RETRY_GAP)
    }

    pub fn notice(&self) -> String {
        let cause = match self.cause {
            RetryCause::Transport => "connection failed",
            RetryCause::RateLimited => "rate limited",
            RetryCause::ServerError => "server error",
            RetryCause::StreamCut => "stream interrupted",
        };
        format!(
            "Reconnecting\u{2026} {}/{MAX_PROVIDER_RETRIES} ({}, {cause}, after {} ms): the failed call may have consumed usage.",
            self.attempt,
            self.role.label(),
            self.delay_ms
        )
    }
}
