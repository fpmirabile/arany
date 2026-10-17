use super::{AgentPhase, Provider, ProviderIdentity};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_PROVIDER_FALLBACKS: usize = 2;
pub const MAX_PROVIDER_SWITCHES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    Primary,
    Child,
    Review,
    Compaction,
}

impl ProviderRole {
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
pub type AdmittedProviders = Providers<Vec<Arc<dyn Provider>>>;

impl AdmittedProviders {
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
                        && (identity.limits.output_token_bound
                            != super::OutputTokenBound::LocalAcceptanceOnly
                            || identity.limits.max_output_tokens <= super::DEFAULT_OUTPUT_TOKEN_CAP)
                })
                && chain
                    .iter()
                    .enumerate()
                    .all(|(index, identity)| !chain[..index].iter().any(|prior| prior == identity))
        })
    }
}

impl ProviderIdentity {
    pub(crate) fn shares_account(&self, other: &Self) -> bool {
        self.profile == other.profile && self.provenance == other.provenance
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFallback {
    pub role: ProviderRole,
    pub call_index: u32,
    pub failure: crate::session::ProviderCallDisposition,
    pub from: ProviderIdentity,
    pub to: ProviderIdentity,
}

impl ProviderFallback {
    pub(crate) fn notice(&self) -> String {
        let role = match self.role {
            ProviderRole::Primary => "primary",
            ProviderRole::Child => "child",
            ProviderRole::Review => "review",
            ProviderRole::Compaction => "compaction",
        };
        format!(
            "Provider fallback ({role}): the failed call may have consumed usage. {}/{} to {}/{}.",
            self.from.profile, self.from.model, self.to.profile, self.to.model
        )
    }
}
