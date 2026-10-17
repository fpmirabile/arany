use crate::cli::attached::run::{Selection, select_provider};
use crate::cli::exec::ProviderArg;
use arany::{
    AdmittedProviders, Effort, MAX_PROVIDER_FALLBACKS, Provider, ProviderRole, Providers,
    RecordKind, StateRoot,
};
use serde::Deserialize;
use std::{path::Path, str::FromStr, sync::Arc};
use uuid::{Uuid, Version};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingConfig {
    version: u8,
    providers: Providers<Option<Vec<Route>>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    provider: String,
    model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    effort: Option<Effort>,
    #[serde(deserialize_with = "Option::deserialize")]
    account_id: Option<Uuid>,
}

impl Route {
    fn validate(&self) -> Result<(), String> {
        let profile = ProviderArg::from_str(&self.provider)?;
        if self.model.is_empty()
            || self.model.len() > 128
            || self.model.chars().any(char::is_control)
            || self
                .account_id
                .is_some_and(|id| id.get_version() != Some(Version::SortRand))
            || matches!(profile, ProviderArg::Custom(_)) && self.account_id.is_some()
        {
            return Err("invalid Provider role route".into());
        }
        match profile {
            ProviderArg::Openai | ProviderArg::Anthropic => {
                arany::resolve_native_effort_for_run(&self.provider, &self.model, self.effort)
                    .map_err(|_| "invalid Provider role model/effort")?;
            }
            ProviderArg::Chatgpt => {
                arany::validate_native_model_id(&self.model)
                    .map_err(|_| "invalid ChatGPT role model")?;
                if self.effort.is_none() || self.account_id.is_none() {
                    return Err("ChatGPT role requires account_id and effort".into());
                }
            }
            ProviderArg::Custom(_) => {}
        }
        Ok(())
    }
}

fn parse(bytes: &[u8]) -> Result<RoutingConfig, String> {
    let config: RoutingConfig =
        serde_json::from_slice(bytes).map_err(|_| "invalid provider-routing.json")?;
    if config.version != 1 {
        return Err("unsupported Provider routing version".into());
    }
    for chain in config.providers.values().into_iter().flatten() {
        if chain.is_empty() || chain.len() > MAX_PROVIDER_FALLBACKS + 1 {
            return Err("Provider role needs one route and at most two fallbacks".into());
        }
        for (index, route) in chain.iter().enumerate() {
            route.validate()?;
            if chain[..index].contains(route) {
                return Err("duplicate Provider role route".into());
            }
        }
    }
    Ok(config)
}

pub(crate) async fn configured_providers(
    state_dir: &Path,
    workspace: &Path,
    primary: Arc<dyn Provider>,
) -> Result<AdmittedProviders, String> {
    let root = StateRoot::admit(state_dir).map_err(|_| "state directory unavailable or unsafe")?;
    let Some(bytes) = root
        .record(RecordKind::ProviderRouting)
        .read()
        .map_err(|_| "Provider routing unavailable or unsafe")?
    else {
        return Ok(Providers::all(vec![primary]));
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
                .map_err(|_| "invalid primary effort")?,
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
            return Err("configured primary route differs from selected Provider".into());
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
    Ok(providers)
}

async fn admit(
    state_dir: &Path,
    workspace: &Path,
    route: &Route,
) -> Result<Arc<dyn Provider>, String> {
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
    .map_err(|error| {
        format!(
            "Provider role route {}/{} refused: {error}",
            route.provider, route.model
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
