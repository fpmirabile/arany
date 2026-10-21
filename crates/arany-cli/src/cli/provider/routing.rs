use super::select::{ProviderArg, Selection, SelectionError, select_provider};
use arany::{
    AdmittedProviders, Effort, MAX_PROVIDER_FALLBACKS, Provider, ProviderRole, Providers,
    RecordFormat, RecordKind, StateRoot,
};
use serde::Deserialize;
use std::{path::Path, str::FromStr, sync::Arc};
use uuid::{Uuid, Version};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoutingError {
    #[error("invalid Provider role route")]
    InvalidRoute,
    #[error("invalid Provider role model/effort")]
    InvalidModelEffort,
    #[error("invalid ChatGPT role model")]
    InvalidChatGptModel,
    #[error("ChatGPT role requires account_id and effort")]
    ChatGptAccountAndEffortRequired,
    #[error("invalid provider-routing.json")]
    InvalidConfiguration,
    #[error("unsupported Provider routing version")]
    UnsupportedVersion,
    #[error("Provider role needs one route and at most two fallbacks")]
    InvalidRouteCount,
    #[error("duplicate Provider role route")]
    DuplicateRoute,
    #[error("state directory unavailable or unsafe")]
    StateUnavailable,
    #[error("Provider routing unavailable or unsafe")]
    RoutingUnavailable,
    #[error("invalid primary effort")]
    InvalidPrimaryEffort,
    #[error("configured primary route differs from selected Provider")]
    PrimarySelectionMismatch,
    #[error("{0}")]
    InvalidProvider(&'static str),
    #[error("Provider role route {provider}/{model} refused: {source}")]
    RouteRefused {
        provider: String,
        model: String,
        #[source]
        source: SelectionError,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingConfig {
    version: u8,
    providers: Providers<Option<Vec<Route>>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Route {
    pub(crate) provider: String,
    pub(crate) model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) effort: Option<Effort>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) account_id: Option<Uuid>,
}

impl Route {
    pub(super) fn validate(&self) -> Result<(), RoutingError> {
        let profile =
            ProviderArg::from_str(&self.provider).map_err(RoutingError::InvalidProvider)?;
        if self.model.is_empty()
            || self.model.len() > 128
            || self.model.chars().any(char::is_control)
            || self
                .account_id
                .is_some_and(|id| id.get_version() != Some(Version::SortRand))
            || matches!(profile, ProviderArg::Custom(_)) && self.account_id.is_some()
        {
            return Err(RoutingError::InvalidRoute);
        }
        match profile {
            ProviderArg::Openai | ProviderArg::Anthropic => {
                arany::resolve_native_effort_for_run(&self.provider, &self.model, self.effort)
                    .map_err(|_| RoutingError::InvalidModelEffort)?;
            }
            ProviderArg::Chatgpt => {
                arany::validate_native_model_id(&self.model)
                    .map_err(|_| RoutingError::InvalidChatGptModel)?;
                if self.effort.is_none() || self.account_id.is_none() {
                    return Err(RoutingError::ChatGptAccountAndEffortRequired);
                }
            }
            ProviderArg::Custom(_) => {}
        }
        Ok(())
    }
}

pub(crate) const PROVIDER_ROUTING: RecordFormat = RecordFormat {
    kind: RecordKind::ProviderRouting,
    field: "version",
    floor: 1,
    current: 1,
    steps: &[],
    accepts: |bytes| parse(bytes).is_ok(),
};

fn parse(bytes: &[u8]) -> Result<RoutingConfig, RoutingError> {
    let config: RoutingConfig =
        serde_json::from_slice(bytes).map_err(|_| RoutingError::InvalidConfiguration)?;
    if config.version != 1 {
        return Err(RoutingError::UnsupportedVersion);
    }
    for chain in config.providers.values().into_iter().flatten() {
        if chain.is_empty() || chain.len() > MAX_PROVIDER_FALLBACKS + 1 {
            return Err(RoutingError::InvalidRouteCount);
        }
        for (index, route) in chain.iter().enumerate() {
            route.validate()?;
            if chain[..index].contains(route) {
                return Err(RoutingError::DuplicateRoute);
            }
        }
    }
    Ok(config)
}

pub(crate) async fn configured_providers(
    state_dir: &Path,
    workspace: &Path,
    primary: Arc<dyn Provider>,
) -> Result<AdmittedProviders, RoutingError> {
    let root = StateRoot::admit(state_dir).map_err(|_| RoutingError::StateUnavailable)?;
    let Some(bytes) = root
        .read_current(&PROVIDER_ROUTING)
        .map_err(|_| RoutingError::RoutingUnavailable)?
    else {
        return Ok(AdmittedProviders::single(primary));
    };
    let config = parse(&bytes)?;
    let mut providers = Providers::all(vec![Arc::clone(&primary)]);
    if let Some(chain) = &config.providers.primary {
        let selected = &chain[0];
        let effort = if matches!(selected.provider.as_str(), "openai" | "anthropic") {
            Some(
                arany::resolve_native_effort_for_run(
                    &selected.provider,
                    &selected.model,
                    selected.effort,
                )
                .map_err(|_| RoutingError::InvalidPrimaryEffort)?,
            )
        } else {
            selected.effort
        };
        let identity = primary.identity();
        let account_id = match &identity.provenance {
            arany::Provenance::Native {
                saved_api_account_id,
            } => *saved_api_account_id,
            arany::Provenance::ChatGpt(provenance) => Some(provenance.account_id),
            arany::Provenance::Custom(_) => None,
        };
        if selected.provider != identity.profile
            || selected.model != identity.model
            || effort != identity.effort
            || selected.account_id != account_id
        {
            return Err(RoutingError::PrimarySelectionMismatch);
        }
        for route in &chain[1..] {
            providers
                .primary
                .push(admit(state_dir, workspace, route).await?);
        }
    }
    for role in [
        ProviderRole::Child,
        ProviderRole::Review,
        ProviderRole::Compaction,
    ] {
        let chain = match config.providers.get(role) {
            None => providers.primary.clone(),
            Some(routes) => {
                let mut chain = Vec::with_capacity(routes.len());
                for route in routes {
                    chain.push(admit(state_dir, workspace, route).await?);
                }
                chain
            }
        };
        *providers.get_mut(role) = chain;
    }
    Ok(AdmittedProviders::from_roles(providers))
}

pub(super) async fn admit(
    state_dir: &Path,
    workspace: &Path,
    route: &Route,
) -> Result<Arc<dyn Provider>, RoutingError> {
    select_provider(
        state_dir,
        Selection {
            workspace,
            profile: &route.provider,
            model: &route.model,
            effort: route.effort,
            account_id: route.account_id,
        },
    )
    .await
    .map_err(|source| RoutingError::RouteRefused {
        provider: route.provider.clone(),
        model: route.model.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_error_display_preserves_cli_text() {
        for (error, expected) in [
            (RoutingError::InvalidRoute, "invalid Provider role route"),
            (
                RoutingError::InvalidModelEffort,
                "invalid Provider role model/effort",
            ),
            (
                RoutingError::InvalidChatGptModel,
                "invalid ChatGPT role model",
            ),
            (
                RoutingError::ChatGptAccountAndEffortRequired,
                "ChatGPT role requires account_id and effort",
            ),
            (
                RoutingError::InvalidConfiguration,
                "invalid provider-routing.json",
            ),
            (
                RoutingError::UnsupportedVersion,
                "unsupported Provider routing version",
            ),
            (
                RoutingError::InvalidRouteCount,
                "Provider role needs one route and at most two fallbacks",
            ),
            (
                RoutingError::DuplicateRoute,
                "duplicate Provider role route",
            ),
            (
                RoutingError::StateUnavailable,
                "state directory unavailable or unsafe",
            ),
            (
                RoutingError::RoutingUnavailable,
                "Provider routing unavailable or unsafe",
            ),
            (RoutingError::InvalidPrimaryEffort, "invalid primary effort"),
            (
                RoutingError::PrimarySelectionMismatch,
                "configured primary route differs from selected Provider",
            ),
            (
                RoutingError::InvalidProvider("invalid Provider"),
                "invalid Provider",
            ),
            (
                RoutingError::InvalidProvider("invalid custom Provider name"),
                "invalid custom Provider name",
            ),
            (
                RoutingError::RouteRefused {
                    provider: "openai".into(),
                    model: "gpt-5.4".into(),
                    source: SelectionError::OpenAiKeyUnavailable,
                },
                "Provider role route openai/gpt-5.4 refused: selected OpenAI API key unavailable",
            ),
        ] {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn routing_configuration_is_closed_and_bounded() {
        let route = serde_json::json!({"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null});
        let valid = serde_json::json!({"version":1,"providers":{"primary":null,"child":[route.clone()],"review":null,"compaction":null}});
        assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for invalid in [
            serde_json::json!({"version":2,"providers":valid["providers"]}),
            serde_json::json!({"version":1,"providers":{"primary":null,"child":[],"review":null,"compaction":null}}),
            serde_json::json!({"version":1,"providers":{"primary":null,"child":[route.clone(),route.clone()],"review":null,"compaction":null}}),
            serde_json::json!({"version":1,"providers":{"primary":null,"child":[route.clone(),route.clone(),route.clone(),route.clone(),route.clone()],"review":null,"compaction":null}}),
            serde_json::json!({"version":1,"providers":{"primary":null,"child":[{"provider":"openai","model":"gpt-5.4","effort":"low"}],"review":null,"compaction":null}}),
            serde_json::json!({"version":1,"providers":valid["providers"],"unknown":true}),
        ] {
            assert!(
                parse(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
    }
}
