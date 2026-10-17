use super::{AgentPhase, Provenance, Provider, ProviderIdentity};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

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
    /// One Provider for every role, with no fallback.
    pub fn single(provider: Arc<dyn Provider>) -> Self {
        Self::all(vec![provider])
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
    /// Whether both identities bill one account. Models and evidence may differ within an account.
    pub(crate) fn shares_account(&self, other: &Self) -> bool {
        self.profile == other.profile && self.account_key() == other.account_key()
    }

    fn account_key(&self) -> AccountKey<'_> {
        match &self.provenance {
            Provenance::Native {
                saved_api_account_id,
            } => AccountKey::Native(*saved_api_account_id),
            Provenance::Custom(custom) => AccountKey::Custom(&custom.endpoint),
            Provenance::ChatGpt(chatgpt) => AccountKey::ChatGpt(chatgpt.account_id),
        }
    }
}

#[derive(PartialEq)]
enum AccountKey<'a> {
    Native(Option<Uuid>),
    Custom(&'a str),
    ChatGpt(Uuid),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatGptAdmission, ChatGptProvenance};

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
    pub role: ProviderRole,
    pub call_index: u32,
    pub failure: crate::session::ProviderCallDisposition,
    pub from: ProviderIdentity,
    pub to: ProviderIdentity,
}

impl ProviderFallback {
    pub fn notice(&self) -> String {
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
