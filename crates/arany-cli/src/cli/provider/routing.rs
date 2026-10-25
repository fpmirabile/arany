use super::select::{ProviderArg, Selection, SelectionError, select_provider};
use arany::{
    AdmittedProviders, Effort, MAX_PROVIDER_FALLBACKS, Provider, ProviderKind, ProviderRecord,
    ProviderRole, Providers, RecordFormat, RecordKind, RecordStep, StateRoot,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, str::FromStr, sync::Arc};
use uuid::{Uuid, Version};

pub(crate) const MAX_ROLE_MODEL_BYTES: usize = 128;

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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingConfig {
    version: u8,
    providers: Providers<Option<Vec<Route>>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Route {
    pub(crate) provider: ProviderRecord,
    pub(crate) model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) effort: Option<Effort>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) account_id: Option<Uuid>,
}

impl Route {
    pub(crate) fn validate(&self) -> Result<(), RoutingError> {
        let profile = ProviderArg::from_str(&self.provider.profile())
            .map_err(RoutingError::InvalidProvider)?;
        if self.model.is_empty()
            || self.model.len() > MAX_ROLE_MODEL_BYTES
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
                arany::resolve_native_effort_for_run(
                    &self.provider.profile(),
                    &self.model,
                    self.effort,
                )
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

pub(crate) struct RoutingDraft {
    pub(crate) providers: Providers<Option<Vec<Route>>>,
    expected: Option<Vec<u8>>,
}

impl RoutingDraft {
    pub(crate) fn load(state: &StateRoot) -> Result<Self, RoutingError> {
        let expected = state
            .read_current(&PROVIDER_ROUTING)
            .map_err(|_| RoutingError::RoutingUnavailable)?;
        let providers = match &expected {
            Some(bytes) => parse(bytes)?.providers,
            None => Providers::all(None),
        };
        Ok(Self {
            providers,
            expected,
        })
    }

    pub(crate) fn expected(&self) -> Option<&[u8]> {
        self.expected.as_deref()
    }

    pub(crate) fn encode(&self, primary: Option<&Route>) -> Result<Vec<u8>, RoutingError> {
        let mut providers = self.providers.clone();
        for chain in [
            &mut providers.primary,
            &mut providers.child,
            &mut providers.review,
            &mut providers.compaction,
        ]
        .into_iter()
        .flatten()
        {
            for route in chain {
                route.resolve_effort()?;
            }
        }
        let bytes = serde_json::to_vec(&RoutingConfig {
            version: PROVIDER_ROUTING.current as u8,
            providers,
        })
        .map_err(|_| RoutingError::InvalidConfiguration)?;
        let config = parse(&bytes)?;
        if config
            .providers
            .primary
            .as_ref()
            .is_some_and(|chain| Some(&chain[0]) != primary)
        {
            return Err(RoutingError::PrimarySelectionMismatch);
        }
        Ok(bytes)
    }

    pub(crate) fn publish(
        &self,
        state: &StateRoot,
        bytes: Option<&[u8]>,
    ) -> Result<(), arany::StoreError> {
        state.edit_provider_record(
            arany::ProviderRecordTarget::Routing,
            self.expected(),
            bytes.map_or(
                arany::ProviderRecordEdit::Remove,
                arany::ProviderRecordEdit::Replace,
            ),
            &PROVIDER_ROUTING,
        )
    }
}

impl Route {
    pub(crate) fn resolve_effort(&mut self) -> Result<(), RoutingError> {
        if self.provider.kind().is_native_api() {
            self.effort = Some(
                arany::resolve_native_effort_for_run(
                    &self.provider.profile(),
                    &self.model,
                    self.effort,
                )
                .map_err(|_| RoutingError::InvalidModelEffort)?,
            );
        }
        Ok(())
    }
}

pub(crate) const PROVIDER_ROUTING: RecordFormat = RecordFormat {
    kind: RecordKind::ProviderRouting,
    field: "version",
    floor: 1,
    current: 2,
    steps: &[RecordStep {
        from: 1,
        transform: migrate_routing,
    }],
    accepts: |bytes| parse(bytes).is_ok(),
};

pub(super) fn migrate_route(route: &mut serde_json::Value) -> Option<()> {
    let provider = ProviderRecord::parse(route.get("provider")?.as_str()?)?;
    route["provider"] = serde_json::to_value(provider).ok()?;
    Some(())
}

fn migrate_routing(mut value: serde_json::Value) -> Option<serde_json::Value> {
    for chain in value.get_mut("providers")?.as_object_mut()?.values_mut() {
        if !chain.is_null() {
            for route in chain.as_array_mut()? {
                migrate_route(route)?;
            }
        }
    }
    value["version"] = PROVIDER_ROUTING.current.into();
    Some(value)
}

pub(crate) fn references_custom_profile(
    state_dir: &Path,
    name: &str,
) -> Result<bool, RoutingError> {
    let state = StateRoot::open_existing(state_dir).map_err(|_| RoutingError::StateUnavailable)?;
    let Some(bytes) = state
        .read_current(&PROVIDER_ROUTING)
        .map_err(|_| RoutingError::RoutingUnavailable)?
    else {
        return Ok(false);
    };
    let config = parse(&bytes)?;
    Ok(config
        .providers
        .values()
        .into_iter()
        .flatten()
        .flatten()
        .any(|route| route.provider.profile() == ProviderKind::custom_profile(name)))
}

fn parse(bytes: &[u8]) -> Result<RoutingConfig, RoutingError> {
    let config: RoutingConfig =
        serde_json::from_slice(bytes).map_err(|_| RoutingError::InvalidConfiguration)?;
    if u64::from(config.version) != PROVIDER_ROUTING.current {
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
        let effort = if selected.provider.kind().is_native_api() {
            Some(
                arany::resolve_native_effort_for_run(
                    &selected.provider.profile(),
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
        if selected.provider.profile() != identity.profile
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
            profile: &route.provider.profile(),
            model: &route.model,
            effort: route.effort,
            account_id: route.account_id,
        },
    )
    .await
    .map_err(|source| RoutingError::RouteRefused {
        provider: route.provider.profile(),
        model: route.model.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_routing_edits_reopen_inheritance_and_refuse_stale_replacement_and_removal() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let mut draft = RoutingDraft::load(&root).unwrap();
        let stale = RoutingDraft::load(&root).unwrap();
        let primary = Route {
            provider: ProviderRecord::parse("openai").unwrap(),
            model: "gpt-5.4".into(),
            effort: Some(Effort::Low),
            account_id: None,
        };
        let fallback = Route {
            provider: ProviderRecord::parse("anthropic").unwrap(),
            model: "claude-sonnet-5".into(),
            effort: Some(Effort::High),
            account_id: None,
        };
        draft.providers.primary = Some(vec![primary.clone(), fallback.clone()]);
        draft.providers.child = Some(vec![fallback.clone()]);
        let bytes = draft.encode(Some(&primary)).unwrap();
        draft.publish(&root, Some(&bytes)).unwrap();
        assert!(matches!(
            stale.publish(&root, Some(&bytes)),
            Err(arany::StoreError::RecordChanged)
        ));
        assert!(matches!(
            stale.publish(&root, None),
            Err(arany::StoreError::RecordChanged)
        ));
        let reopened = RoutingDraft::load(&StateRoot::open_existing(root.path()).unwrap()).unwrap();
        assert_eq!(
            reopened.providers.primary,
            Some(vec![primary.clone(), fallback.clone()])
        );
        assert_eq!(reopened.providers.child, Some(vec![fallback.clone()]));
        assert_eq!(reopened.providers.review, None);
        assert_eq!(reopened.providers.compaction, None);
        assert!(matches!(
            reopened.encode(Some(&fallback)),
            Err(RoutingError::PrimarySelectionMismatch)
        ));
        assert!(matches!(
            root.record(RecordKind::ProviderRouting).replace(&bytes),
            Err(arany::StoreError::ReadOnly)
        ));
        reopened.publish(&root, None).unwrap();
        assert!(RoutingDraft::load(&root).unwrap().expected().is_none());
        draft.providers.primary = Some(vec![
            fallback.clone(),
            Route {
                effort: None,
                ..fallback.clone()
            },
        ]);
        assert!(matches!(
            draft.encode(Some(&fallback)),
            Err(RoutingError::DuplicateRoute)
        ));
    }

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
        let route = serde_json::json!({"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":"low","account_id":null});
        let valid = serde_json::json!({"version":2,"providers":{"primary":null,"child":[route.clone()],"review":null,"compaction":null}});
        assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
        let mut legacy = valid.clone();
        legacy["version"] = 1.into();
        legacy["providers"]["child"][0]["provider"] = "custom:exact-name".into();
        let upgraded = PROVIDER_ROUTING
            .upgrade(&serde_json::to_vec(&legacy).unwrap())
            .unwrap();
        let reopened = parse(&upgraded).unwrap();
        assert_eq!(
            reopened.providers.child.unwrap()[0].provider.profile(),
            "custom:exact-name"
        );
        for provider in [
            serde_json::json!({"kind":"custom","custom_name":null}),
            serde_json::json!({"kind":"openai","custom_name":"exact-name"}),
        ] {
            let mut invalid = valid.clone();
            invalid["providers"]["child"][0]["provider"] = provider;
            assert!(parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        for invalid in [
            serde_json::json!({"version":3,"providers":valid["providers"]}),
            serde_json::json!({"version":2,"providers":{"primary":null,"child":[],"review":null,"compaction":null}}),
            serde_json::json!({"version":2,"providers":{"primary":null,"child":[route.clone(),route.clone()],"review":null,"compaction":null}}),
            serde_json::json!({"version":2,"providers":{"primary":null,"child":[route.clone(),route.clone(),route.clone(),route.clone(),route.clone()],"review":null,"compaction":null}}),
            serde_json::json!({"version":2,"providers":{"primary":null,"child":[{"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":"low"}],"review":null,"compaction":null}}),
            serde_json::json!({"version":2,"providers":valid["providers"],"unknown":true}),
        ] {
            assert!(
                parse(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
    }
}
