use super::routing::ROLES;
use super::{
    SetupError, SetupJournal, await_setup_ack, choose, read_setup_value, review_warning, routing,
};
use crate::cli::provider::roster::{
    MAX_ELIGIBLE_PROFILES, MAX_PRESETS, MAX_PROFILES, Preset, Profile, RosterDraft,
};
use crate::cli::provider::routing::{Route, RoutingDraft};
use crate::terminal::AttachedTerminal;
use arany::{
    MAX_CUSTOM_PROFILE_NAME_BYTES, MAX_PROVIDER_FALLBACKS, ProviderRole, ProviderSetupTargetKind,
    Providers, RecordKind, SessionDefaults, StateRoot,
};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) async fn manage(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    journal: &mut SetupJournal,
    current: &SessionDefaults,
) -> Result<(), SetupError> {
    let state = StateRoot::admit(state_dir).map_err(|error| error.to_string())?;
    let mut draft = RosterDraft::load(&state)?;
    let primary = routing::current_route(current)?;
    loop {
        let choices = roster_choices(terminal, &state).await?;
        let Some(action) = choose(
            terminal,
            1,
            "Roster profiles",
            "Private draft; Save does not activate a preset; launch with --profile PRESET",
            &choices,
        )
        .await?
        else {
            return Ok(());
        };
        match action {
            b'1' => {
                let result = profiles(terminal, &state, &mut draft, primary.as_ref()).await;
                retain_draft(terminal, result).await?;
            }
            b'2' => {
                let result = presets(terminal, &mut draft).await;
                retain_draft(terminal, result).await?;
            }
            b'3' => match draft.preview() {
                Ok(preview) => {
                    await_setup_ack(terminal, "Roster preview", &preview).await?;
                }
                Err(message) => {
                    await_setup_ack(terminal, "Invalid roster draft", &message).await?;
                }
            },
            b'4' => match save(terminal, &state, journal, &draft).await {
                Ok(true) => return Ok(()),
                result => retain_draft(terminal, result.map(|_| ())).await?,
            },
            b'5' => {
                let result = match RoutingDraft::load(&state) {
                    Ok(routing) => routing::clear(terminal, &state, journal, &routing)
                        .await
                        .map(|_| ()),
                    Err(error) => Err(error.to_string().into()),
                };
                retain_draft(terminal, result).await?;
            }
            _ => return Ok(()),
        }
    }
}

async fn roster_choices(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
) -> Result<Vec<(u8, &'static str)>, SetupError> {
    let mut choices = vec![
        (b'1', "Route profiles"),
        (b'2', "Named presets"),
        (b'3', "Preview complete roster"),
        (b'4', "Save roster"),
    ];
    match state.record(RecordKind::ProviderRouting).present() {
        Ok(true) => choices.push((b'5', "Clear role routing")),
        Ok(false) => {}
        Err(error) => retain_draft(terminal, Err(error.to_string().into())).await?,
    }
    choices.push((b'6', "Back; discard draft"));
    Ok(choices)
}

async fn retain_draft(
    terminal: &mut AttachedTerminal,
    result: Result<(), SetupError>,
) -> Result<(), SetupError> {
    match result {
        Err(SetupError::Recoverable(message)) => {
            await_setup_ack(terminal, "Roster draft retained", &message).await?;
            Ok(())
        }
        other => other,
    }
}

async fn save(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    journal: &mut SetupJournal,
    draft: &RosterDraft,
) -> Result<bool, SetupError> {
    let prepared = draft
        .encode()
        .and_then(|bytes| Ok((bytes, draft.preview()?)));
    let (bytes, mut preview) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            await_setup_ack(terminal, "Invalid roster draft", &message).await?;
            return Ok(false);
        }
    };
    if state
        .record(RecordKind::ProviderRouting)
        .present()
        .map_err(|error| error.to_string())?
    {
        preview.push_str("\nRole routing is present: --profile will refuse until you separately Clear role routing. Save does not clear it.\n");
    }
    if !review_warning(
        terminal,
        "Save roster?",
        &preview,
        "Save the entire roster record?",
    )
    .await?
    {
        return Ok(false);
    }
    publish(journal, state, draft, bytes.as_deref()).await?;
    await_setup_ack(terminal, "Roster saved", &launch_guidance(draft)).await?;
    Ok(true)
}

fn launch_guidance(draft: &RosterDraft) -> String {
    if draft.presets.is_empty() {
        return "Removed the roster record; current conversation selection is unchanged".into();
    }
    let mut text =
        "Saved; current conversation selection is unchanged. Launch explicitly:\n".to_owned();
    for preset in &draft.presets {
        text.push_str(&format!("arany --profile {}\n", preset.name));
    }
    text
}

async fn named_choice<'a>(
    terminal: &mut AttachedTerminal,
    title: &str,
    names: impl Iterator<Item = &'a str>,
    can_add: bool,
) -> Result<Option<u8>, SetupError> {
    let mut choices = names
        .enumerate()
        .map(|(index, name)| {
            (
                u8::try_from(index + 1).expect("bounded roster names"),
                name.to_owned(),
            )
        })
        .collect::<Vec<_>>();
    if can_add {
        choices.push((b'a', "Add new".into()));
    }
    choices.push((b'b', "Back".into()));
    let rows = choices
        .iter()
        .map(|(key, label)| (*key, label.as_str()))
        .collect::<Vec<_>>();
    choose(
        terminal,
        1,
        title,
        "Choose a named draft entry; Back keeps the roster draft",
        &rows,
    )
    .await
}

async fn read_name(terminal: &mut AttachedTerminal) -> Result<Option<String>, SetupError> {
    let Some(name) = read_setup_value(
        terminal,
        "Roster name",
        "Lowercase letters, digits and hyphens; begin with a letter",
        "",
        MAX_CUSTOM_PROFILE_NAME_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    if !arany::valid_profile_name(&name) {
        return Err("Invalid roster name".into());
    }
    Ok(Some(name))
}

async fn profiles(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    draft: &mut RosterDraft,
    primary: Option<&Route>,
) -> Result<(), SetupError> {
    loop {
        let action = named_choice(
            terminal,
            "Route profiles",
            draft.profiles.iter().map(|profile| profile.name.as_str()),
            draft.profiles.len() < MAX_PROFILES,
        )
        .await?;
        match action {
            Some(b'a') => {
                let Some(name) = read_name(terminal).await? else {
                    continue;
                };
                if draft.profiles.iter().any(|profile| profile.name == name) {
                    await_setup_ack(terminal, "Route profiles", "Duplicate profile name").await?;
                    continue;
                }
                if let Some(routes) = edit_chain(terminal, state, vec![], primary).await? {
                    draft.profiles.push(Profile { name, routes });
                }
            }
            None | Some(b'b') => return Ok(()),
            Some(key) => {
                let index = usize::from(key.saturating_sub(1));
                profile_action(terminal, state, draft, (index, primary)).await?;
            }
        }
    }
}

async fn profile_action(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    draft: &mut RosterDraft,
    (index, primary): (usize, Option<&Route>),
) -> Result<(), SetupError> {
    let profile = &draft.profiles[index];
    match choose(
        terminal,
        1,
        "Profile action",
        &profile.name,
        &[
            (b'1', "Edit ordered chain"),
            (b'2', "Remove profile"),
            (b'3', "Back"),
        ],
    )
    .await?
    {
        Some(b'1') => {
            if let Some(routes) =
                edit_chain(terminal, state, profile.routes.clone(), primary).await?
            {
                draft.profiles[index].routes = routes;
            }
        }
        Some(b'2') => {
            if let Err(message) = draft.remove_profile(index) {
                await_setup_ack(terminal, "Profile referenced", &message).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn edit_chain(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    mut chain: Vec<Route>,
    primary: Option<&Route>,
) -> Result<Option<Vec<Route>>, SetupError> {
    loop {
        let mut choices = chain
            .iter()
            .enumerate()
            .map(|(index, route)| {
                (
                    u8::try_from(index + 1).expect("bounded chain"),
                    routing::tuple_label(route),
                )
            })
            .collect::<Vec<_>>();
        if chain.len() < MAX_PROVIDER_FALLBACKS + 1 {
            choices.push((b'a', "Append route".into()));
        }
        if !chain.is_empty() {
            choices.push((b'd', "Done; keep chain in draft".into()));
        }
        let rows = choices
            .iter()
            .map(|(key, label)| (*key, label.as_str()))
            .collect::<Vec<_>>();
        match choose(
            terminal,
            1,
            "Roster chain",
            "Initial route then ordered fallbacks; Escape discards this chain edit",
            &rows,
        )
        .await?
        {
            None => return Ok(None),
            Some(b'd') => return Ok(Some(chain)),
            Some(b'a') => {
                if let Some(route) = routing::select_route(terminal, state, primary).await? {
                    if chain.contains(&route) {
                        await_setup_ack(terminal, "Roster chain", "Duplicate route").await?;
                    } else {
                        chain.push(route);
                    }
                }
            }
            Some(key) => {
                let index = usize::from(key.saturating_sub(1));
                routing::edit_slot(
                    terminal,
                    state,
                    &mut chain,
                    (ProviderRole::Child, index),
                    primary,
                )
                .await?;
            }
        }
    }
}

async fn presets(
    terminal: &mut AttachedTerminal,
    draft: &mut RosterDraft,
) -> Result<(), SetupError> {
    loop {
        let action = named_choice(
            terminal,
            "Named presets",
            draft.presets.iter().map(|preset| preset.name.as_str()),
            draft.presets.len() < MAX_PRESETS,
        )
        .await?;
        match action {
            None | Some(b'b') => return Ok(()),
            Some(b'a') => {
                let Some(first) = draft.profiles.first() else {
                    await_setup_ack(terminal, "Named presets", "Add a route profile first").await?;
                    continue;
                };
                let primary = first.name.clone();
                let Some(name) = read_name(terminal).await? else {
                    continue;
                };
                if draft.presets.iter().any(|preset| preset.name == name) {
                    await_setup_ack(terminal, "Named presets", "Duplicate preset name").await?;
                    continue;
                }
                let mut providers = Providers::all(None);
                providers.primary = Some(primary.clone());
                let preset = Preset {
                    name,
                    providers,
                    child_profiles: vec![primary],
                };
                if let Some(preset) = edit_preset(terminal, draft, preset).await? {
                    draft.presets.push(preset);
                }
            }
            Some(key) => {
                let index = usize::from(key.saturating_sub(1));
                preset_action(terminal, draft, index).await?;
            }
        }
    }
}

async fn preset_action(
    terminal: &mut AttachedTerminal,
    draft: &mut RosterDraft,
    index: usize,
) -> Result<(), SetupError> {
    match choose(
        terminal,
        1,
        "Preset action",
        &draft.presets[index].name,
        &[
            (b'1', "Edit roles and eligible children"),
            (b'2', "Remove preset"),
            (b'3', "Back"),
        ],
    )
    .await?
    {
        Some(b'1') => {
            if let Some(preset) = edit_preset(terminal, draft, draft.presets[index].clone()).await?
            {
                draft.presets[index] = preset;
            }
        }
        Some(b'2') => {
            draft.presets.remove(index);
        }
        _ => {}
    }
    Ok(())
}

async fn edit_preset(
    terminal: &mut AttachedTerminal,
    draft: &RosterDraft,
    mut preset: Preset,
) -> Result<Option<Preset>, SetupError> {
    loop {
        let mut choices = ROLES
            .iter()
            .enumerate()
            .map(|(index, (role, label))| {
                (
                    u8::try_from(index + 1).expect("four roles"),
                    format!(
                        "{label}: {}",
                        preset
                            .providers
                            .get(*role)
                            .as_deref()
                            .unwrap_or("Inherit primary")
                    ),
                )
            })
            .collect::<Vec<_>>();
        choices.push((
            b'c',
            format!("Eligible children: {}", preset.child_profiles.join(", ")),
        ));
        choices.push((b'd', "Done; keep preset in draft".into()));
        let rows = choices
            .iter()
            .map(|(key, label)| (*key, label.as_str()))
            .collect::<Vec<_>>();
        match choose(terminal, 1, "Preset roles", &preset.name, &rows).await? {
            None => return Ok(None),
            Some(b'd') => return Ok(Some(preset)),
            Some(b'c') => edit_children(terminal, draft, &mut preset).await?,
            Some(key) => {
                let (role, _) = ROLES[usize::from(key.saturating_sub(1))];
                if let Some(name) = profile_reference(terminal, draft, role).await? {
                    *preset.providers.get_mut(role) = name;
                }
            }
        }
    }
}

async fn profile_reference(
    terminal: &mut AttachedTerminal,
    draft: &RosterDraft,
    role: ProviderRole,
) -> Result<Option<Option<String>>, SetupError> {
    let mut choices = draft
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            (
                u8::try_from(index + 1).expect("bounded profile count"),
                profile.name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    if role != ProviderRole::Primary {
        choices.push((b'i', "Inherit primary"));
    }
    Ok(
        match choose(
            terminal,
            1,
            "Role profile",
            "Choose a named chain",
            &choices,
        )
        .await?
        {
            None => None,
            Some(b'i') => Some(None),
            Some(key) => Some(Some(
                draft.profiles[usize::from(key.saturating_sub(1))]
                    .name
                    .clone(),
            )),
        },
    )
}

async fn edit_children(
    terminal: &mut AttachedTerminal,
    draft: &RosterDraft,
    preset: &mut Preset,
) -> Result<(), SetupError> {
    let mut eligible = preset.child_profiles.clone();
    loop {
        let mut choices = draft
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                (
                    u8::try_from(index + 1).expect("bounded profile count"),
                    format!(
                        "{}: {}",
                        profile.name,
                        if eligible.contains(&profile.name) {
                            "eligible"
                        } else {
                            "excluded"
                        }
                    ),
                )
            })
            .collect::<Vec<_>>();
        choices.push((b'd', "Done; keep eligible children".into()));
        let rows = choices
            .iter()
            .map(|(key, label)| (*key, label.as_str()))
            .collect::<Vec<_>>();
        match choose(
            terminal,
            1,
            "Eligible children",
            "Toggle profiles; include the resolved child default; Escape discards these toggles",
            &rows,
        )
        .await?
        {
            None => return Ok(()),
            Some(b'd') => {
                preset.child_profiles = eligible;
                return Ok(());
            }
            Some(key) => {
                let name = &draft.profiles[usize::from(key.saturating_sub(1))].name;
                if let Some(index) = eligible.iter().position(|other| other == name) {
                    eligible.remove(index);
                } else if eligible.len() < MAX_ELIGIBLE_PROFILES {
                    eligible.push(name.clone());
                } else {
                    await_setup_ack(
                        terminal,
                        "Eligible children",
                        "Eligible profile limit reached",
                    )
                    .await?;
                }
            }
        }
    }
}

async fn publish(
    journal: &mut SetupJournal,
    state: &StateRoot,
    draft: &RosterDraft,
    bytes: Option<&[u8]>,
) -> Result<(), SetupError> {
    let operation = journal
        .start(
            ProviderSetupTargetKind::Roster,
            "roster".into(),
            draft.expected().map(|value| Sha256::digest(value).into()),
        )
        .await?;
    journal
        .finish_record(operation, bytes, draft.publish(state, bytes), "Roster")
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{Event, ProviderSetupDisposition, SessionView, Store};

    fn populate(draft: &mut RosterDraft) {
        draft.profiles.push(Profile {
            name: "main".into(),
            routes: vec![Route {
                provider: arany::ProviderRecord::parse("openai").unwrap(),
                model: "gpt-5.4".into(),
                effort: None,
                account_id: None,
            }],
        });
        draft.profiles.push(Profile {
            name: "inspect".into(),
            routes: draft.profiles[0].routes.clone(),
        });
        for name in ["coding", "research"] {
            draft.presets.push(Preset {
                name: name.into(),
                providers: Providers {
                    primary: Some("main".into()),
                    child: Some("inspect".into()),
                    review: None,
                    compaction: None,
                },
                child_profiles: vec!["inspect".into()],
            });
        }
    }

    #[tokio::test]
    async fn roster_edits_replay_save_conflict_and_final_removal_without_activation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).unwrap();
        let mut journal = SetupJournal::new(&path, &workspace, None);
        let mut draft = RosterDraft::load(&state).unwrap();
        assert!(draft.encode().is_err());
        populate(&mut draft);
        assert!(
            draft
                .remove_profile(0)
                .unwrap_err()
                .contains("coding, research")
        );
        let bytes = draft.encode().unwrap().unwrap();
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            record["version"],
            crate::cli::provider::roster::PROVIDER_ROSTER.current
        );
        assert_eq!(
            record["profiles"][0]["routes"][0]["provider"],
            serde_json::json!({"kind":"openai","custom_name":null})
        );
        let preview = draft.preview().unwrap();
        for expected in [
            "arany --profile coding",
            "arany --profile research",
            "child=inspect",
            "effort=none",
        ] {
            assert!(preview.contains(expected));
        }
        assert!(
            publish(&mut journal, &state, &draft, Some(&bytes))
                .await
                .is_ok()
        );
        assert!(
            publish(&mut journal, &state, &draft, Some(&bytes))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(path.join("provider-roster.json")).unwrap(),
            bytes
        );
        assert!(matches!(
            state.record(RecordKind::ProviderRoster).replace(&bytes),
            Err(arany::StoreError::ReadOnly)
        ));
        assert!(matches!(
            state.record(RecordKind::ProviderRoster).remove(),
            Err(arany::StoreError::ReadOnly)
        ));
        let mut reopened = RosterDraft::load(&state).unwrap();
        reopened.presets[0].child_profiles.clear();
        assert!(reopened.encode().is_err());
        reopened.presets.clear();
        assert!(reopened.encode().unwrap().is_none());
        assert!(publish(&mut journal, &state, &reopened, None).await.is_ok());
        let store = Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        let id = journal.session_id().unwrap();
        let events = store.load_session(id).await.unwrap();
        store.close().await.unwrap();
        assert_edit_replay(id, &events);
        assert!(!path.join("provider-roster.json").exists());
        assert!(RosterDraft::load(&state).unwrap().expected().is_none());
    }
    fn assert_edit_replay(id: arany::SessionId, events: &[arany::EventEnvelope]) {
        let view = SessionView::replay(id, events).unwrap().unwrap();
        assert!(view.runs.is_empty());
        assert_eq!(view.defaults, SessionDefaults::default());
        assert!(!view.provider_setup_blocks_continuation());
        let outcomes = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ProviderSetupEditStarted { target, .. } => {
                    assert_eq!(target.kind, ProviderSetupTargetKind::Roster);
                    None
                }
                Event::ProviderSetupEditFinished { disposition, .. } => Some(*disposition),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes,
            [
                ProviderSetupDisposition::Applied,
                ProviderSetupDisposition::Failed,
                ProviderSetupDisposition::Applied
            ]
        );
    }
}
