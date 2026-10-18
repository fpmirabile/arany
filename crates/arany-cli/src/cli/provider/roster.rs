use super::routing::{Route, RoutingError, admit};
use super::select::{ProviderArg, Selection};
use arany::{
    AdmittedProviders, AdmittedRoster, Effort, NamedProviderRoutes, Provider, Providers,
    RecordKind, RosterSnapshot, StateRoot, valid_profile_name,
};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

const INVALID: &str = "invalid or oversized provider-roster.json";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    name: String,
    routes: Vec<Route>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preset {
    name: String,
    providers: Providers<Option<String>>,
    child_profiles: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u8,
    profiles: Vec<Profile>,
    presets: Vec<Preset>,
}

#[derive(Clone)]
pub(crate) struct SelectedRoster {
    name: String,
    profiles: Vec<Profile>,
    providers: Providers<String>,
    child_profiles: Vec<String>,
}

fn bounded(bytes: &[u8]) -> bool {
    if bytes.len() > 64 * 1024 {
        return false;
    }
    let (mut depth, mut string, mut escape) = (0u32, false, false);
    for byte in bytes {
        if string {
            if escape {
                escape = false;
            } else if *byte == b'\\' {
                escape = true;
            } else if *byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 16 {
                        return false;
                    }
                }
                b'}' | b']' => {
                    let Some(next) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = next;
                }
                _ => (),
            }
        }
    }
    depth == 0 && !string
}

fn parse(bytes: &[u8], selected: &str) -> Result<SelectedRoster, String> {
    if !bounded(bytes) || !valid_profile_name(selected) {
        return Err(INVALID.into());
    }
    let mut config: Config = serde_json::from_slice(bytes).map_err(|_| INVALID)?;
    if config.version != 1
        || config.profiles.is_empty()
        || config.profiles.len() > 32
        || config.presets.is_empty()
        || config.presets.len() > 16
    {
        return Err(INVALID.into());
    }
    let mut names = BTreeSet::new();
    for profile in &mut config.profiles {
        if !valid_profile_name(&profile.name)
            || !names.insert(profile.name.clone())
            || profile.routes.is_empty()
            || profile.routes.len() > 3
        {
            return Err(INVALID.into());
        }
        for index in 0..profile.routes.len() {
            let route = &mut profile.routes[index];
            route.validate().map_err(|_| INVALID)?;
            if matches!(route.provider.as_str(), "openai" | "anthropic") {
                route.effort = Some(
                    arany::resolve_native_effort_for_run(
                        &route.provider,
                        &route.model,
                        route.effort,
                    )
                    .map_err(|_| INVALID)?,
                );
            }
            if profile.routes[..index].contains(&profile.routes[index]) {
                return Err(INVALID.into());
            }
        }
    }
    let mut presets = BTreeSet::new();
    let mut result = None;
    for preset in config.presets {
        if !valid_profile_name(&preset.name)
            || !presets.insert(preset.name.clone())
            || preset.child_profiles.is_empty()
            || preset.child_profiles.len() > 8
        {
            return Err(INVALID.into());
        }
        let primary = preset.providers.primary.ok_or(INVALID)?;
        let providers = Providers {
            child: preset.providers.child.unwrap_or_else(|| primary.clone()),
            review: preset.providers.review.unwrap_or_else(|| primary.clone()),
            compaction: preset
                .providers
                .compaction
                .unwrap_or_else(|| primary.clone()),
            primary,
        };
        let mut eligible = BTreeSet::new();
        if preset
            .child_profiles
            .iter()
            .any(|name| !eligible.insert(name.clone()))
            || !preset.child_profiles.contains(&providers.child)
        {
            return Err(INVALID.into());
        }
        let referenced: BTreeSet<_> = providers
            .values()
            .into_iter()
            .chain(preset.child_profiles.iter())
            .cloned()
            .collect();
        if referenced.len() > 8 || !referenced.is_subset(&names) {
            return Err(INVALID.into());
        }
        if preset.name == selected {
            result = Some(SelectedRoster {
                name: preset.name,
                profiles: referenced
                    .iter()
                    .map(|name| {
                        config
                            .profiles
                            .iter()
                            .find(|profile| &profile.name == name)
                            .expect("validated reference")
                            .clone()
                    })
                    .collect(),
                providers,
                child_profiles: preset.child_profiles,
            });
        }
    }
    result.ok_or_else(|| "selected roster preset is unavailable".into())
}

pub(crate) fn load(
    state_dir: &Path,
    workspace: &Path,
    name: &str,
) -> Result<SelectedRoster, String> {
    let root = StateRoot::admit(state_dir).map_err(|_| "state directory unavailable or unsafe")?;
    super::super::admit_workspace(Some(workspace.to_path_buf()), Some(state_dir.to_path_buf()))
        .map_err(str::to_owned)?;
    if root
        .record(RecordKind::ProviderRouting)
        .read()
        .map_err(|_| "Provider routing unavailable or unsafe")?
        .is_some()
    {
        return Err(
            "--profile conflicts with provider-routing.json; choose one routing source".into(),
        );
    }
    let bytes = root
        .record(RecordKind::ProviderRoster)
        .read()
        .map_err(|_| "Provider roster unavailable or unsafe")?
        .ok_or("provider-roster.json is unavailable")?;
    parse(&bytes, name)
}

impl SelectedRoster {
    pub(crate) fn primary(&self) -> &Route {
        &self
            .profiles
            .iter()
            .find(|profile| profile.name == self.providers.primary)
            .expect("resolved primary")
            .routes[0]
    }

    pub(crate) fn matches(
        &self,
        provider: Option<&ProviderArg>,
        model: Option<&str>,
        effort: Option<Effort>,
    ) -> Result<(), String> {
        let primary = self.primary();
        if provider.is_some_and(|provider| provider.profile_label() != primary.provider)
            || model.is_some_and(|model| model != primary.model)
            || effort.is_some_and(|effort| Some(effort) != primary.effort)
        {
            return Err("explicit Provider/model/effort differs from --profile primary".into());
        }
        Ok(())
    }

    pub(crate) fn matches_selection(&self, selection: Selection<'_>) -> Result<(), String> {
        let primary = self.primary();
        let effort = if matches!(selection.profile, "openai" | "anthropic") {
            Some(
                arany::resolve_native_effort_for_run(
                    selection.profile,
                    selection.model,
                    selection.effort,
                )
                .map_err(|_| "invalid Provider/model/effort".to_owned())?,
            )
        } else {
            selection.effort
        };
        if selection.profile != primary.provider
            || selection.model != primary.model
            || effort != primary.effort
            || selection.account_id != primary.account_id
        {
            return Err("selection differs from --profile; use its primary tuple or start without --profile".into());
        }
        Ok(())
    }

    pub(crate) fn manifest(&self) -> Result<String, String> {
        let mut text = format!(
            "Roster preset: {}\nRoles: primary={}, child={}, review={}, compaction={}\nEligible child profiles: {}\n",
            self.name,
            self.providers.primary,
            self.providers.child,
            self.providers.review,
            self.providers.compaction,
            self.child_profiles.join(", ")
        );
        for profile in &self.profiles {
            for (index, route) in profile.routes.iter().enumerate() {
                let billing = match route.provider.as_str() {
                    "chatgpt" => {
                        "ChatGPT plan; local-only output cap does not cap remote plan consumption"
                    }
                    "openai" | "anthropic" => "native API",
                    _ => "exact custom; unknown pricing",
                };
                text.push_str(&format!(
                    "  {}[{}]: {}/{} effort={} ({})\n",
                    profile.name,
                    index,
                    route.provider,
                    crate::presentation::escape_terminal(&route.model),
                    route
                        .effort
                        .map_or_else(|| "default".into(), |effort| effort.to_string()),
                    billing
                ));
            }
        }
        if text.len() > 16 * 1024 {
            return Err("roster manifest exceeds 16 KiB".into());
        }
        Ok(text)
    }

    pub(crate) async fn admit(
        &self,
        state_dir: &Path,
        workspace: &Path,
    ) -> Result<AdmittedProviders, String> {
        tokio::time::timeout(
            Duration::from_secs(120),
            self.admit_inner(state_dir, workspace),
        )
        .await
        .map_err(|_| "roster admission exceeded 120 seconds".to_owned())?
    }

    async fn admit_inner(
        &self,
        state_dir: &Path,
        workspace: &Path,
    ) -> Result<AdmittedProviders, String> {
        let mut cache: Vec<(Route, Arc<dyn Provider>)> = Vec::new();
        let mut profiles = Vec::new();
        let mut identities = Vec::new();
        for profile in &self.profiles {
            let mut chain = Vec::new();
            for (index, route) in profile.routes.iter().enumerate() {
                let provider = if let Some((_, provider)) =
                    cache.iter().find(|(cached, _)| cached == route)
                {
                    Arc::clone(provider)
                } else {
                    let provider = admit(state_dir, workspace, route).await.map_err(|error| {
                        format!(
                            "roster profile {} route {} refused: {error}",
                            profile.name, index
                        )
                    })?;
                    cache.push((route.clone(), Arc::clone(&provider)));
                    provider
                };
                if chain
                    .iter()
                    .any(|prior: &Arc<dyn Provider>| prior.identity() == provider.identity())
                {
                    return Err(INVALID.into());
                }
                chain.push(provider);
            }
            identities.push(NamedProviderRoutes {
                name: profile.name.clone(),
                routes: chain
                    .iter()
                    .map(|provider| provider.identity().clone())
                    .collect(),
            });
            profiles.push((profile.name.clone(), chain));
        }
        let mut snapshot = RosterSnapshot {
            preset: self.name.clone(),
            selection_digest: [0; 32],
            profiles: identities,
            providers: self.providers.clone(),
            child_profiles: self.child_profiles.clone(),
        };
        snapshot.selection_digest = snapshot.digest().ok_or(INVALID)?;
        if !snapshot.valid() {
            return Err("roster snapshot is invalid or exceeds 16 KiB".into());
        }
        let chain = |name: &str| {
            profiles
                .iter()
                .find(|(candidate, _)| candidate == name)
                .expect("admitted reference")
                .1
                .clone()
        };
        Ok(AdmittedProviders {
            roles: Providers {
                primary: chain(&self.providers.primary),
                child: chain(&self.providers.child),
                review: chain(&self.providers.review),
                compaction: chain(&self.providers.compaction),
            },
            roster: Some(AdmittedRoster { snapshot, profiles }),
        })
    }
}

pub(crate) async fn pinned_compaction(
    state_dir: &Path,
    workspace: &Path,
    config: &arany::RunConfig,
) -> Result<AdmittedProviders, String> {
    let roster = config.roster.as_ref().ok_or(INVALID)?;
    let routes = roster.chain(&roster.providers.compaction).ok_or(INVALID)?;
    let mut chain = Vec::new();
    for identity in routes {
        let route = Route {
            provider: identity.profile.clone(),
            model: identity.model.clone(),
            effort: identity.effort,
            account_id: match &identity.provenance {
                arany::Provenance::Native {
                    saved_api_account_id,
                } => *saved_api_account_id,
                arany::Provenance::ChatGpt(p) => Some(p.account_id),
                arany::Provenance::Custom(_) => None,
            },
        };
        chain.push(
            admit(state_dir, workspace, &route)
                .await
                .map_err(|error: RoutingError| error.to_string())?,
        );
    }
    let mut providers = AdmittedProviders::single(Arc::clone(&chain[0]));
    providers.compaction = chain;
    Ok(providers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn record() -> Value {
        json!({"version":1,"profiles":[{"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":null,"account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]})
    }

    #[test]
    fn roster_configuration_is_closed_and_bounded() {
        let valid = record();
        let bytes = serde_json::to_vec(&valid).unwrap();
        assert!(parse(&bytes, "coding").is_ok());
        assert!(parse(&bytes, "missing").is_err());
        for (pointer, value) in [
            ("/version", json!(2)),
            ("/extra", json!(true)),
            ("/profiles/0/extra", json!(true)),
            ("/profiles/0/name", json!("../main")),
            (
                "/profiles/0/routes/0/endpoint",
                json!("https://example.com"),
            ),
            (
                "/profiles/0/routes/0/account_id",
                json!("00000000-0000-4000-8000-000000000000"),
            ),
            ("/profiles/0/routes/0/provider", json!("forged")),
            ("/presets/0/name", json!("Coding")),
            ("/presets/0/providers/primary", Value::Null),
            ("/presets/0/providers/child", json!("missing")),
            ("/presets/0/child_profiles", json!([])),
            ("/presets/0/child_profiles", json!(["main", "main"])),
            ("/presets/0/child_profiles", json!(["missing"])),
        ] {
            let mut invalid = valid.clone();
            if let Some(slot) = invalid.pointer_mut(pointer) {
                *slot = value;
            } else if pointer == "/extra" {
                invalid["extra"] = value;
            } else if pointer == "/profiles/0/extra" {
                invalid["profiles"][0]["extra"] = value;
            } else {
                invalid["profiles"][0]["routes"][0]["endpoint"] = value;
            }
            assert!(
                parse(&serde_json::to_vec(&invalid).unwrap(), "coding").is_err(),
                "{pointer}"
            );
        }
        for pointer in [
            "/profiles/0/routes/0/effort",
            "/profiles/0/routes/0/account_id",
            "/presets/0/providers/child",
        ] {
            let mut invalid = valid.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            invalid
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(parse(&serde_json::to_vec(&invalid).unwrap(), "coding").is_err());
        }
        let source = String::from_utf8(bytes.clone()).unwrap();
        for (from, to) in [
            ("\"version\":1", "\"version\":1,\"version\":1"),
            ("\"name\":\"main\"", "\"name\":\"main\",\"name\":\"main\""),
            ("\"child\":null", "\"child\":null,\"child\":null"),
            ("\"effort\":null", "\"effort\":null,\"effort\":null"),
        ] {
            assert!(source.contains(from));
            assert!(parse(source.replace(from, to).as_bytes(), "coding").is_err());
        }
        let mut exact = bytes;
        exact.resize(64 * 1024, b' ');
        assert!(parse(&exact, "coding").is_ok());
        exact.push(b' ');
        assert!(parse(&exact, "coding").is_err());
        assert!(bounded(
            format!("{}0{}", "[".repeat(16), "]".repeat(16)).as_bytes()
        ));
        assert!(!bounded(
            format!("{}0{}", "[".repeat(17), "]".repeat(17)).as_bytes()
        ));
        for name in ["", "a/b", "a\n", "A", "é", &"a".repeat(65)] {
            assert!(!valid_profile_name(name));
        }
        assert!(valid_profile_name(&"a".repeat(64)));

        let mut max = valid.clone();
        max["profiles"] = json!(
            (0..32)
                .map(|index| {
                    let mut profile = valid["profiles"][0].clone();
                    profile["name"] = json!(format!("profile-{index}"));
                    profile
                })
                .collect::<Vec<_>>()
        );
        max["presets"] = json!((0..16).map(|index| json!({"name":format!("preset-{index}"),"providers":{"primary":"profile-0","child":null,"review":null,"compaction":null},"child_profiles":["profile-0"]})).collect::<Vec<_>>());
        assert!(parse(&serde_json::to_vec(&max).unwrap(), "preset-0").is_ok());
        for field in ["profiles", "presets"] {
            let mut invalid = max.clone();
            let mut extra = invalid[field][0].clone();
            extra["name"] = json!("extra");
            invalid[field].as_array_mut().unwrap().push(extra);
            assert!(parse(&serde_json::to_vec(&invalid).unwrap(), "preset-0").is_err());
        }
        for count in [8, 9] {
            let mut expanded = max.clone();
            expanded["presets"][0]["child_profiles"] = json!(
                (0..count)
                    .map(|index| format!("profile-{index}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                parse(&serde_json::to_vec(&expanded).unwrap(), "preset-0").is_ok(),
                count == 8
            );
        }
        for field in ["profiles", "presets"] {
            let mut duplicate = valid.clone();
            duplicate[field]
                .as_array_mut()
                .unwrap()
                .push(valid[field][0].clone());
            assert!(parse(&serde_json::to_vec(&duplicate).unwrap(), "coding").is_err());
        }
        for count in [3, 4] {
            let mut chained = valid.clone();
            chained["profiles"][0]["routes"] = json!((0..count).map(|index| json!({"provider":"openai","model":format!("model-{index}"),"effort":"low","account_id":null})).collect::<Vec<_>>());
            assert_eq!(
                parse(&serde_json::to_vec(&chained).unwrap(), "coding").is_ok(),
                count == 3
            );
        }
        let mut duplicate = valid.clone();
        duplicate["profiles"][0]["routes"]
            .as_array_mut()
            .unwrap()
            .push(valid["profiles"][0]["routes"][0].clone());
        assert!(parse(&serde_json::to_vec(&duplicate).unwrap(), "coding").is_err());
    }

    #[test]
    fn roster_manifest_and_explicit_tuple_use_resolved_defaults() {
        let selected = parse(&serde_json::to_vec(&record()).unwrap(), "coding").unwrap();
        assert_eq!(
            selected.manifest().unwrap(),
            "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main\n  main[0]: openai/gpt-5.4 effort=none (native API)\n"
        );
        assert!(
            selected
                .matches(
                    Some(&ProviderArg::Openai),
                    Some("gpt-5.4"),
                    Some(Effort::None)
                )
                .is_ok()
        );
        assert!(
            selected
                .matches(Some(&ProviderArg::Anthropic), None, None)
                .is_err()
        );
        assert!(selected.matches(None, Some("other"), None).is_err());
        assert!(selected.matches(None, None, Some(Effort::Low)).is_err());
        let mut metadata = record();
        metadata["profiles"][0]["routes"][0]["effort"] = json!("none");
        assert_eq!(
            selected.primary(),
            parse(&serde_json::to_vec(&metadata).unwrap(), "coding")
                .unwrap()
                .primary()
        );
        metadata["profiles"][0]["routes"][0] = json!({"provider":"chatgpt","model":"visible-model","effort":"high","account_id":uuid::Uuid::now_v7()});
        let subscription = parse(&serde_json::to_vec(&metadata).unwrap(), "coding").unwrap();
        assert_eq!(
            subscription.manifest().unwrap(),
            "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main\n  main[0]: chatgpt/visible-model effort=high (ChatGPT plan; local-only output cap does not cap remote plan consumption)\n"
        );
        let names: Vec<_> = (0..8)
            .map(|index| format!("{}{index}", "a".repeat(63)))
            .collect();
        metadata["profiles"] = json!(names.iter().map(|name| json!({"name":name,"routes":(0..3).map(|index| json!({"provider":format!("custom:{}", "a".repeat(64)),"model":format!("{}{index:02}", "\u{202e}".repeat(42)),"effort":null,"account_id":null})).collect::<Vec<_>>()})).collect::<Vec<_>>());
        metadata["presets"][0]["providers"]["primary"] = json!(names[0]);
        metadata["presets"][0]["child_profiles"] = json!(names);
        let maximum = parse(&serde_json::to_vec(&metadata).unwrap(), "coding")
            .unwrap()
            .manifest()
            .unwrap();
        assert!(maximum.len() <= 16 * 1024);
        assert!(!maximum.contains('\u{202e}'));
        assert_eq!(
            maximum
                .lines()
                .filter(|line| line.starts_with("  "))
                .count(),
            24
        );
    }
}
