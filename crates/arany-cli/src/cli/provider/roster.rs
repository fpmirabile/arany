use super::routing::{Route, RoutingError, admit};
use super::select::{ProviderArg, Selection};
use arany::{
    AdmittedProviders, AdmittedRoster, Effort, NamedProviderRoutes, Provider, ProviderKind,
    Providers, RecordFormat, RecordKind, RosterSnapshot, StateRoot, valid_profile_name,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

pub(crate) const MAX_PROFILES: usize = 32;
pub(crate) const MAX_PRESETS: usize = 16;
pub(crate) const MAX_ELIGIBLE_PROFILES: usize = 8;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;

const INVALID: &str = "invalid or oversized provider-roster.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) routes: Vec<Route>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) providers: Providers<Option<String>>,
    pub(crate) child_profiles: Vec<String>,
}

impl Preset {
    fn bounded_references(&self) -> Result<BTreeSet<String>, String> {
        let referenced = self
            .providers
            .values()
            .into_iter()
            .flatten()
            .chain(self.child_profiles.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        if referenced.len() > MAX_ELIGIBLE_PROFILES {
            return Err(format!(
                "Preset {} references more than {MAX_ELIGIBLE_PROFILES} distinct profiles; remove a role reference or an eligible child",
                crate::presentation::escape_terminal(&self.name),
            ));
        }
        Ok(referenced)
    }

    fn validate_child_default(&self) -> Result<(), String> {
        let primary = self.providers.primary.as_ref().ok_or(INVALID)?;
        let child = self.providers.child.as_ref().unwrap_or(primary);
        if !self.child_profiles.contains(child) {
            return Err(format!(
                "Preset {} must list its child default {} under eligible children",
                crate::presentation::escape_terminal(&self.name),
                crate::presentation::escape_terminal(child),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
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
    if bytes.len() > RecordKind::ProviderRoster.limit() {
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

pub(crate) const PROVIDER_ROSTER: RecordFormat = RecordFormat {
    kind: RecordKind::ProviderRoster,
    accepts: |bytes| presets(bytes).is_ok(),
};

pub(crate) struct RosterDraft {
    pub(crate) profiles: Vec<Profile>,
    pub(crate) presets: Vec<Preset>,
    expected: Option<Vec<u8>>,
}

impl RosterDraft {
    pub(crate) fn load(state: &StateRoot) -> Result<Self, String> {
        let expected = state
            .record(PROVIDER_ROSTER.kind)
            .read()
            .map_err(|_| "Provider roster unavailable or unsafe")?;
        let config = match &expected {
            Some(bytes) => {
                presets(bytes)?;
                serde_json::from_slice::<Config>(bytes).map_err(|_| INVALID)?
            }
            None => Config {
                profiles: vec![],
                presets: vec![],
            },
        };
        Ok(Self {
            profiles: config.profiles,
            presets: config.presets,
            expected,
        })
    }

    pub(crate) fn expected(&self) -> Option<&[u8]> {
        self.expected.as_deref()
    }

    pub(crate) fn remove_profile(&mut self, index: usize) -> Result<(), String> {
        let profile = self.profiles.get(index).ok_or(INVALID)?;
        let uses = self
            .presets
            .iter()
            .filter(|preset| {
                preset
                    .providers
                    .values()
                    .into_iter()
                    .any(|name| name.as_ref() == Some(&profile.name))
                    || preset.child_profiles.contains(&profile.name)
            })
            .map(|preset| preset.name.as_str())
            .collect::<Vec<_>>();
        if !uses.is_empty() {
            return Err(format!(
                "Profile is referenced by presets: {}; change those references first",
                uses.join(", ")
            ));
        }
        self.profiles.remove(index);
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Option<Vec<u8>>, String> {
        if self.presets.is_empty() {
            return if self.expected.is_some() {
                Ok(None)
            } else {
                Err("Add a named preset before saving route profiles".into())
            };
        }
        for preset in &self.presets {
            preset.validate_child_default()?;
            preset.bounded_references()?;
        }
        let mut profiles = self.profiles.clone();
        for profile in &mut profiles {
            for route in &mut profile.routes {
                route.resolve_effort().map_err(|_| INVALID)?;
            }
        }
        let bytes = serde_json::to_vec(&Config {
            profiles,
            presets: self.presets.clone(),
        })
        .map_err(|_| INVALID)?;
        presets(&bytes)?;
        Ok(Some(bytes))
    }

    pub(crate) fn preview(&self) -> Result<String, String> {
        let Some(bytes) = self.encode()? else {
            return Ok("No presets remain. Save removes the entire roster record, including its route profiles. No preset is activated.".into());
        };
        let mut text = "Private roster draft. Save does not activate a preset, check a route or contact a Provider. Every candidate is re-admitted at submission.\n".to_owned();
        for profile in &self.profiles {
            text.push_str(&format!("\nProfile: {}\n", profile.name));
            for (index, route) in profile.routes.iter().enumerate() {
                text.push_str(&format!(
                    "  {index}: {}/{} effort={} account={}\n",
                    route.provider.profile(),
                    crate::presentation::escape_terminal(&route.model),
                    route
                        .effort
                        .map_or("default".into(), |effort| effort.to_string()),
                    route
                        .account_id
                        .map_or("environment".into(), |id| id.to_string())
                ));
            }
        }
        for preset in &self.presets {
            text.push_str(&format!(
                "\nConfigured references for {}: primary={}, child={}, review={}, compaction={}\n",
                preset.name,
                preset.providers.primary.as_deref().ok_or(INVALID)?,
                preset
                    .providers
                    .child
                    .as_deref()
                    .unwrap_or("Inherit primary"),
                preset
                    .providers
                    .review
                    .as_deref()
                    .unwrap_or("Inherit primary"),
                preset
                    .providers
                    .compaction
                    .as_deref()
                    .unwrap_or("Inherit primary")
            ));
        }
        for selected in presets(&bytes)? {
            text.push('\n');
            text.push_str(&selected.manifest()?);
            text.push_str(&format!(
                "Launch explicitly: arany --profile {}\n",
                selected.name
            ));
        }
        Ok(text)
    }

    pub(crate) fn publish(
        &self,
        state: &StateRoot,
        bytes: Option<&[u8]>,
    ) -> Result<(), arany::StoreError> {
        state.edit_provider_record(
            arany::ProviderRecordTarget::Roster,
            self.expected(),
            bytes.map_or(
                arany::ProviderRecordEdit::Remove,
                arany::ProviderRecordEdit::Replace,
            ),
            &PROVIDER_ROSTER,
        )
    }
}

pub(crate) fn references_custom_profile(state_dir: &Path, name: &str) -> Result<bool, String> {
    let state = StateRoot::open_existing(state_dir).map_err(|_| "state directory unavailable")?;
    let Some(bytes) = state
        .record(PROVIDER_ROSTER.kind)
        .read()
        .map_err(|_| "Provider roster unavailable or unsafe")?
    else {
        return Ok(false);
    };
    presets(&bytes)?;
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
    let provider = ProviderKind::custom_profile(name);
    Ok(config
        .profiles
        .iter()
        .flat_map(|profile| &profile.routes)
        .any(|route| route.provider.profile() == provider))
}

fn parse(bytes: &[u8], selected: &str) -> Result<SelectedRoster, String> {
    if !valid_profile_name(selected) {
        return Err(INVALID.into());
    }
    presets(bytes)?
        .into_iter()
        .find(|preset| preset.name == selected)
        .ok_or_else(|| "selected roster preset is unavailable".into())
}

fn presets(bytes: &[u8]) -> Result<Vec<SelectedRoster>, String> {
    if !bounded(bytes) {
        return Err(INVALID.into());
    }
    let mut config: Config = serde_json::from_slice(bytes).map_err(|_| INVALID)?;
    if config.profiles.is_empty()
        || config.profiles.len() > MAX_PROFILES
        || config.presets.is_empty()
        || config.presets.len() > MAX_PRESETS
    {
        return Err(INVALID.into());
    }
    let names = validate_profiles(&mut config.profiles)?;
    let mut preset_names = BTreeSet::new();
    let mut result = Vec::with_capacity(config.presets.len());
    for preset in config.presets {
        if !valid_profile_name(&preset.name)
            || !preset_names.insert(preset.name.clone())
            || preset.child_profiles.is_empty()
            || preset.child_profiles.len() > MAX_ELIGIBLE_PROFILES
        {
            return Err(INVALID.into());
        }
        preset.validate_child_default().map_err(|_| INVALID)?;
        let referenced = preset.bounded_references().map_err(|_| INVALID)?;
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
        {
            return Err(INVALID.into());
        }
        if !referenced.is_subset(&names) {
            return Err(INVALID.into());
        }
        result.push(SelectedRoster {
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
    Ok(result)
}

// Profile names are unique, and each route chain is bounded, valid and free of repeats; a native
// route pins its resolved effort.
fn validate_profiles(profiles: &mut [Profile]) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    for profile in profiles {
        if !valid_profile_name(&profile.name)
            || !names.insert(profile.name.clone())
            || profile.routes.is_empty()
            || profile.routes.len() > arany::MAX_PROVIDER_FALLBACKS + 1
        {
            return Err(INVALID.into());
        }
        for index in 0..profile.routes.len() {
            let route = &mut profile.routes[index];
            route.validate().map_err(|_| INVALID)?;
            if route.provider.kind().is_native_api() {
                route.effort = Some(
                    arany::resolve_native_effort_for_run(
                        &route.provider.profile(),
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
    Ok(names)
}

pub(crate) fn load(
    state_dir: &Path,
    workspace: &Path,
    name: &str,
) -> Result<SelectedRoster, String> {
    super::super::admit_workspace(Some(workspace.to_path_buf()), Some(state_dir.to_path_buf()))
        .map_err(str::to_owned)?;
    load_without_workspace(state_dir, name)
}

pub(crate) fn load_without_workspace(
    state_dir: &Path,
    name: &str,
) -> Result<SelectedRoster, String> {
    let root =
        StateRoot::open_existing(state_dir).map_err(|_| "state directory unavailable or unsafe")?;
    if root
        .record(RecordKind::ProviderRouting)
        .present()
        .map_err(|_| "Provider routing unavailable or unsafe")?
    {
        return Err(
            "--profile conflicts with provider-routing.json; choose one routing source".into(),
        );
    }
    let bytes = root
        .record(PROVIDER_ROSTER.kind)
        .read()
        .map_err(|_| "Provider roster unavailable or unsafe")?
        .ok_or("provider-roster.json is unavailable")?;
    parse(&bytes, name)
}

impl SelectedRoster {
    pub(crate) async fn authenticate(&self, state_dir: &Path) -> bool {
        tokio::time::timeout(Duration::from_secs(120), async {
            for profile in &self.profiles {
                for route in &profile.routes {
                    if !super::select::authenticate(
                        state_dir,
                        &route.provider.profile(),
                        &route.model,
                        route.effort,
                        route.account_id,
                    )
                    .await
                    {
                        return false;
                    }
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }

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
        if provider.is_some_and(|provider| provider.profile_label() != primary.provider.profile())
            || model.is_some_and(|model| model != primary.model)
            || effort.is_some_and(|effort| Some(effort) != primary.effort)
        {
            return Err("explicit Provider/model/effort differs from --profile primary".into());
        }
        Ok(())
    }

    pub(crate) fn matches_selection(&self, selection: Selection<'_>) -> Result<(), String> {
        let primary = self.primary();
        let effort =
            if ProviderKind::parse(selection.profile).is_some_and(ProviderKind::is_native_api) {
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
        if selection.profile != primary.provider.profile()
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
                let billing = match route.provider.kind() {
                    ProviderKind::ChatGpt => {
                        "ChatGPT plan; local-only output cap does not cap remote plan consumption"
                    }
                    ProviderKind::OpenAi | ProviderKind::Anthropic => "native API",
                    ProviderKind::Custom => "exact custom; unknown pricing",
                };
                text.push_str(&format!(
                    "  {}[{}]: {}/{} effort={} ({})\n",
                    profile.name,
                    index,
                    route.provider.profile(),
                    crate::presentation::escape_terminal(&route.model),
                    route
                        .effort
                        .map_or_else(|| "default".into(), |effort| effort.to_string()),
                    billing
                ));
            }
        }
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "roster manifest exceeds {} KiB",
                MAX_MANIFEST_BYTES / 1024
            ));
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
            provider: arany::ProviderRecord::parse(&identity.profile).ok_or(INVALID)?,
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
    providers.roles.compaction = chain;
    Ok(providers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn record() -> Value {
        json!({"profiles":[{"name":"main","routes":[{"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":null,"account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]})
    }

    #[test]
    fn custom_references_include_unselected_profiles_and_reject_invalid_rosters() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().canonicalize().unwrap().join("state");
        let root = StateRoot::admit(&state_dir).unwrap();
        assert!(!references_custom_profile(root.path(), "example-route").unwrap());
        let path = root.path().join("provider-roster.json");
        let mut config = record();
        config["profiles"].as_array_mut().unwrap().push(json!({
            "name": "alternative",
            "routes": [{"provider":{"kind":"custom","custom_name":"example-route"},"model":"example-model","effort":null,"account_id":null}]
        }));
        for selected in [false, true] {
            if selected {
                config["presets"][0]["providers"]["primary"] = json!("alternative");
                config["presets"][0]["child_profiles"] = json!(["alternative"]);
            }
            let bytes = serde_json::to_vec(&config).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(references_custom_profile(root.path(), "example-route").unwrap());
            assert!(!references_custom_profile(root.path(), "other-route").unwrap());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        config["presets"][0]["providers"]["primary"] = json!("missing");
        let bytes = serde_json::to_vec(&config).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(references_custom_profile(root.path(), "example-route").is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
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
            } else if let Some(key @ ("extra" | "version")) = pointer.strip_prefix('/') {
                invalid[key] = value;
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
            ("\"profiles\":[", "\"profiles\":[],\"profiles\":["),
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
            chained["profiles"][0]["routes"] = json!((0..count).map(|index| json!({"provider":{"kind":"openai","custom_name":null},"model":format!("model-{index}"),"effort":"low","account_id":null})).collect::<Vec<_>>());
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
    fn roster_draft_names_the_ineligible_child_default_and_preserves_repairable_edits() {
        let mut config: Config = serde_json::from_value(record()).unwrap();
        let mut alternative = config.profiles[0].clone();
        alternative.name = "inspect".into();
        config.profiles.push(alternative);
        for (primary, child) in [("inspect", None), ("main", Some("inspect"))] {
            let mut preset = config.presets[0].clone();
            preset.providers.primary = Some(primary.into());
            preset.providers.child = child.map(str::to_owned);
            let bytes = serde_json::to_vec(&Config {
                profiles: config.profiles.clone(),
                presets: vec![preset.clone()],
            })
            .unwrap();
            assert!(matches!(presets(&bytes), Err(message) if message == INVALID));
            let mut draft = RosterDraft {
                profiles: config.profiles.clone(),
                presets: vec![preset],
                expected: None,
            };
            let expected =
                "Preset coding must list its child default inspect under eligible children";
            assert_eq!(draft.encode().unwrap_err(), expected);
            assert_eq!(draft.preview().unwrap_err(), expected);
            draft.presets[0].child_profiles.push("inspect".into());
            assert!(draft.encode().unwrap().is_some());
            draft.presets[0].name = "coding\u{001b}".into();
            draft.presets[0].providers.child = Some("unsafe\nchild".into());
            let error = draft.encode().unwrap_err();
            assert!(!error.contains(['\u{001b}', '\n']));
        }
    }

    #[test]
    fn roster_draft_names_excess_references_and_preserves_both_repair_paths() {
        let source: Config = serde_json::from_value(record()).unwrap();
        let profiles = (0..=MAX_ELIGIBLE_PROFILES)
            .map(|index| {
                let mut profile = source.profiles[0].clone();
                profile.name = format!("profile-{index}");
                profile
            })
            .collect::<Vec<_>>();
        for role in [arany::ProviderRole::Review, arany::ProviderRole::Compaction] {
            let mut preset = source.presets[0].clone();
            preset.providers.primary = Some(profiles[0].name.clone());
            preset.child_profiles = profiles[..MAX_ELIGIBLE_PROFILES]
                .iter()
                .map(|profile| profile.name.clone())
                .collect();
            *preset.providers.get_mut(role) = Some(profiles[MAX_ELIGIBLE_PROFILES].name.clone());
            let bytes = serde_json::to_vec(&Config {
                profiles: profiles.clone(),
                presets: vec![preset.clone()],
            })
            .unwrap();
            assert!(matches!(presets(&bytes), Err(message) if message == INVALID));
            let mut draft = RosterDraft {
                profiles: profiles.clone(),
                presets: vec![preset],
                expected: None,
            };
            let expected = format!(
                "Preset coding references more than {MAX_ELIGIBLE_PROFILES} distinct profiles; remove a role reference or an eligible child"
            );
            assert_eq!(draft.encode().unwrap_err(), expected);
            assert_eq!(draft.preview().unwrap_err(), expected);
            draft.presets[0].name.push('\u{001b}');
            assert!(!draft.encode().unwrap_err().contains('\u{001b}'));
            draft.presets[0].name.pop();
            *draft.presets[0].providers.get_mut(role) = None;
            assert!(draft.encode().unwrap().is_some());
            *draft.presets[0].providers.get_mut(role) =
                Some(profiles[MAX_ELIGIBLE_PROFILES].name.clone());
            draft.presets[0].child_profiles.pop();
            assert!(draft.encode().unwrap().is_some());
        }
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
        metadata["profiles"][0]["routes"][0] = json!({"provider":{"kind":"chatgpt","custom_name":null},"model":"visible-model","effort":"high","account_id":uuid::Uuid::now_v7()});
        let subscription = parse(&serde_json::to_vec(&metadata).unwrap(), "coding").unwrap();
        assert_eq!(
            subscription.manifest().unwrap(),
            "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main\n  main[0]: chatgpt/visible-model effort=high (ChatGPT plan; local-only output cap does not cap remote plan consumption)\n"
        );
        let names: Vec<_> = (0..8)
            .map(|index| format!("{}{index}", "a".repeat(63)))
            .collect();
        metadata["profiles"] = json!(names.iter().map(|name| json!({"name":name,"routes":(0..3).map(|index| json!({"provider":{"kind":"custom","custom_name":"a".repeat(64)},"model":format!("{}{index:02}", "\u{202e}".repeat(42)),"effort":null,"account_id":null})).collect::<Vec<_>>()})).collect::<Vec<_>>());
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
