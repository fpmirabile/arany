use super::{
    SetupError, SetupJournal, await_operation, await_setup_ack, choose, read_setup_value,
    review_warning,
};
use crate::accounts::{chatgpt, credentials};
use crate::cli::provider::routing::{MAX_ROLE_MODEL_BYTES, Route, RoutingDraft};
use crate::presentation::escape_terminal;
use crate::terminal::AttachedTerminal;
use arany::{
    CustomProfile, Effort, MAX_PROVIDER_FALLBACKS, ProviderKind, ProviderRecord, ProviderRole,
    ProviderSetupDisposition, ProviderSetupTargetKind, SessionDefaults, StateRoot, StoreError,
};
use sha2::{Digest, Sha256};
use std::path::Path;

const MAX_ROLE_MANIFEST_BYTES: usize = 16 * 1024;
const ROLES: [(ProviderRole, &str); 4] = [
    (ProviderRole::Primary, "Primary"),
    (ProviderRole::Child, "Child"),
    (ProviderRole::Review, "Review"),
    (ProviderRole::Compaction, "Compaction"),
];

pub(super) async fn manage(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    journal: &mut SetupJournal,
    current: &SessionDefaults,
) -> Result<(), SetupError> {
    let state = StateRoot::admit(state_dir).map_err(|error| error.to_string())?;
    let mut draft = RoutingDraft::load(&state).map_err(|error| error.to_string())?;
    let primary = current_route(current)?;
    loop {
        let Some(action) = choose(
            terminal,
            1,
            "Role routing",
            "Draft changes apply only after Save; next submission re-admits every route",
            &[
                (b'1', "Inspect resolved routes"),
                (b'2', "Edit primary"),
                (b'3', "Edit child"),
                (b'4', "Edit review"),
                (b'5', "Edit compaction"),
                (b'6', "Save"),
                (b'7', "Clear role routing"),
                (b'8', "Back; discard draft"),
            ],
        )
        .await?
        else {
            return Ok(());
        };
        match action {
            b'1' => {
                let text = manifest(&state, &draft, primary.as_ref()).await?;
                await_setup_ack(terminal, "Resolved role routes", &text).await?;
            }
            b'2'..=b'5' => {
                let (role, label) = ROLES[usize::from(action - b'2')];
                let edit = edit_chain(terminal, &state, &draft, role, primary.as_ref()).await?;
                let chain = match edit {
                    ChainEdit::Cancelled => continue,
                    ChainEdit::Inherit => None,
                    ChainEdit::Explicit(chain) => Some(chain),
                };
                *draft.providers.get_mut(role) = chain;
                await_setup_ack(terminal, label, "Draft updated; Save publishes all four roles").await?;
            }
            b'6' => {
                let bytes = draft.encode(primary.as_ref()).map_err(|error| error.to_string())?;
                let text = manifest(&state, &draft, primary.as_ref()).await?;
                if review_warning(terminal, "Save role routing?", &text, "Save all four role chains?").await? {
                    publish(journal, &state, &draft, Some(&bytes)).await?;
                    await_setup_ack(terminal, "Role routing", "Saved; next submission re-admits every route; no Provider check was started").await?;
                    return Ok(());
                }
            }
            b'7' => {
                if review_warning(terminal, "Clear role routing?",
                    "Remove the saved role-routing record. Every role will inherit the next selected primary without fallbacks. Active and historical Run pins are unchanged. Roster presets still require explicit --profile selection.",
                    "Clear role routing?").await? {
                    publish(journal, &state, &draft, None).await?;
                    await_setup_ack(terminal, "Role routing", "Cleared; the next submission uses the selected primary").await?;
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

fn current_route(defaults: &SessionDefaults) -> Result<Option<Route>, SetupError> {
    let (Some(provider), Some(model)) = (&defaults.provider, &defaults.model) else {
        return Ok(None);
    };
    let mut route = Route {
        provider: ProviderRecord::parse(provider).ok_or("invalid Provider")?,
        model: model.clone(),
        effort: defaults.effort,
        account_id: defaults.account_id,
    };
    route.resolve_effort().map_err(|error| error.to_string())?;
    route.validate().map_err(|error| error.to_string())?;
    Ok(Some(route))
}

enum ChainEdit {
    Cancelled,
    Inherit,
    Explicit(Vec<Route>),
}

fn chain_choices(chain: &[Route], role: ProviderRole) -> Vec<(u8, String)> {
    let mut choices = vec![(
        b'i',
        if role == ProviderRole::Primary {
            "Use current /model selection".to_owned()
        } else {
            "Inherit primary".to_owned()
        },
    )];
    for (index, route) in chain.iter().enumerate() {
        choices.push((
            u8::try_from(index + 1).unwrap_or(u8::MAX),
            format!("{}: {}", index + 1, tuple_label(route)),
        ));
    }
    if chain.len() < MAX_PROVIDER_FALLBACKS + 1 {
        choices.push((
            b'a',
            if chain.is_empty() {
                "Set initial route"
            } else {
                "Append fallback"
            }
            .into(),
        ));
    }
    if !chain.is_empty() {
        choices.push((b'd', "Done; keep chain in draft".into()));
    }
    choices
}

async fn edit_chain(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    draft: &RoutingDraft,
    role: ProviderRole,
    primary: Option<&Route>,
) -> Result<ChainEdit, SetupError> {
    let mut chain = draft.providers.get(role).clone().unwrap_or_default();
    loop {
        let choices = chain_choices(&chain, role);
        let rows = choices
            .iter()
            .map(|(key, label)| (*key, label.as_str()))
            .collect::<Vec<_>>();
        let Some(action) = choose(
            terminal,
            1,
            "Edit role chain",
            "Ordered routes; primary's first tuple follows /model; Escape discards this chain edit",
            &rows,
        )
        .await?
        else {
            return Ok(ChainEdit::Cancelled);
        };
        match action {
            b'i' => return Ok(ChainEdit::Inherit),
            b'd' => return Ok(ChainEdit::Explicit(chain)),
            b'a' => {
                let route = if role == ProviderRole::Primary && chain.is_empty() {
                    primary.cloned().ok_or(
                        "Select a primary model with /model before configuring its fallbacks",
                    )?
                } else {
                    let Some(route) = select_route(terminal, state, primary).await? else {
                        continue;
                    };
                    route
                };
                if chain.contains(&route) {
                    return Err("duplicate Provider role route".into());
                }
                chain.push(route);
            }
            _ => {
                let index = usize::from(action.saturating_sub(1));
                if index < chain.len() {
                    edit_slot(terminal, state, &mut chain, (role, index), primary).await?;
                }
            }
        }
    }
}

async fn edit_slot(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    chain: &mut Vec<Route>,
    (role, index): (ProviderRole, usize),
    primary: Option<&Route>,
) -> Result<(), SetupError> {
    if role == ProviderRole::Primary && index == 0 {
        let selected = primary.ok_or("Select a primary model with /model first")?;
        if choose(
            terminal,
            1,
            "Primary route",
            &tuple_label(selected),
            &[
                (b'1', "Keep draft tuple"),
                (b'2', "Use current /model tuple"),
            ],
        )
        .await?
            == Some(b'2')
        {
            if chain[1..].contains(selected) {
                return Err("duplicate Provider role route".into());
            }
            chain[0] = selected.clone();
        }
        return Ok(());
    }
    let mut choices = vec![(b'1', "Replace exact tuple")];
    if chain.len() > 1 {
        choices.push((b'2', "Remove route"));
    }
    if index > usize::from(role == ProviderRole::Primary) {
        choices.push((b'3', "Move earlier"));
    }
    if index + 1 < chain.len() {
        choices.push((b'4', "Move later"));
    }
    match choose(
        terminal,
        1,
        "Route action",
        &tuple_label(&chain[index]),
        &choices,
    )
    .await?
    {
        Some(b'1') => {
            if let Some(route) = select_route(terminal, state, primary).await? {
                if chain
                    .iter()
                    .enumerate()
                    .any(|(slot, other)| slot != index && other == &route)
                {
                    return Err("duplicate Provider role route".into());
                }
                chain[index] = route;
            }
        }
        Some(b'2') => {
            chain.remove(index);
        }
        Some(b'3') => chain.swap(index, index - 1),
        Some(b'4') => chain.swap(index, index + 1),
        _ => {}
    }
    Ok(())
}

async fn select_route(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    primary: Option<&Route>,
) -> Result<Option<Route>, SetupError> {
    let Some(source) = choose(
        terminal,
        1,
        "Route source",
        "Select an exact source; Save never checks admission",
        &[
            (b'1', "Current primary tuple"),
            (b'2', "OpenAI API environment"),
            (b'3', "Anthropic API environment"),
            (b'4', "Saved native API account; explicit inspection"),
            (b'5', "Selected ChatGPT account"),
            (b'6', "Custom declaration"),
        ],
    )
    .await?
    else {
        return Ok(None);
    };
    let route = match source {
        b'1' => {
            return primary
                .cloned()
                .map(Some)
                .ok_or_else(|| "Select a primary with /model first".into());
        }
        b'2' | b'3' => Route {
            provider: ProviderRecord::parse(if source == b'2' {
                "openai"
            } else {
                "anthropic"
            })
            .expect("native Provider"),
            model: String::new(),
            effort: None,
            account_id: None,
        },
        b'4' => {
            let Some(route) = saved_native_route(terminal).await? else {
                return Ok(None);
            };
            route
        }
        b'5' => Route {
            provider: ProviderRecord::parse("chatgpt").expect("native Provider"),
            model: String::new(),
            effort: None,
            account_id: Some(chatgpt::selected_account_id().map_err(|error| error.to_string())?),
        },
        b'6' => return select_custom(terminal, state).await,
        _ => return Ok(None),
    };
    edit_model_effort(terminal, route).await
}

async fn saved_native_route(terminal: &mut AttachedTerminal) -> Result<Option<Route>, SetupError> {
    let Some(inspected) = await_operation(
        terminal,
        super::AUTHORIZE_TITLE,
        super::AUTHORIZE_PROMPT,
        credentials::inspect_interactive(),
    )
    .await?
    else {
        return Ok(None);
    };
    let account = inspected
        .map_err(|error| error.to_string())?
        .account
        .ok_or("No saved native API account; add one in API accounts")?;
    Ok(Some(Route {
        provider: account.provider,
        model: account.model,
        effort: account.effort,
        account_id: Some(account.id),
    }))
}

async fn edit_model_effort(
    terminal: &mut AttachedTerminal,
    mut route: Route,
) -> Result<Option<Route>, SetupError> {
    let Some(model) = read_setup_value(
        terminal,
        "Role model",
        "Exact model identifier; unknown native models require an explicit effort",
        &route.model,
        MAX_ROLE_MODEL_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    route.model = model;
    arany::validate_native_model_id(&route.model).map_err(|_| "invalid Provider role model")?;
    let efforts = std::iter::once(None)
        .chain(Effort::ALL.into_iter().map(Some))
        .filter(|effort| {
            if route.provider.kind() == ProviderKind::ChatGpt {
                effort.is_some()
            } else {
                arany::resolve_native_effort_for_run(
                    &route.provider.profile(),
                    &route.model,
                    *effort,
                )
                .is_ok()
            }
        })
        .collect::<Vec<_>>();
    let Some(effort) = choose_effort(terminal, &efforts).await? else {
        return Ok(None);
    };
    route.effort = effort;
    route.validate().map_err(|error| error.to_string())?;
    route.resolve_effort().map_err(|error| error.to_string())?;
    Ok(Some(route))
}

async fn select_custom(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
) -> Result<Option<Route>, SetupError> {
    let (_, profiles) =
        CustomProfile::load_all_with_snapshot(state).map_err(|error| error.to_string())?;
    let statuses = CustomProfile::check_statuses(state, &profiles)
        .await
        .map_err(|error| error.to_string())?;
    let choices = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            (
                u8::try_from(index + 1).unwrap_or(u8::MAX),
                format!(
                    "{} / {} · {}",
                    escape_terminal(profile.name()),
                    escape_terminal(profile.model()),
                    super::custom_status_label(&statuses, profile.name())
                ),
            )
        })
        .collect::<Vec<_>>();
    if choices.is_empty() {
        await_setup_ack(
            terminal,
            "Custom routes",
            "No declarations; add one in Custom routes",
        )
        .await?;
        return Ok(None);
    }
    let rows = choices
        .iter()
        .map(|(key, label)| (*key, label.as_str()))
        .collect::<Vec<_>>();
    let Some(key) = choose(
        terminal,
        1,
        "Custom role route",
        "Saving never checks the route",
        &rows,
    )
    .await?
    else {
        return Ok(None);
    };
    let profile = profiles
        .get(usize::from(key.saturating_sub(1)))
        .ok_or("invalid custom route choice")?;
    let efforts = std::iter::once(None)
        .chain(profile.efforts().iter().copied().map(Some))
        .collect::<Vec<_>>();
    let Some(effort) = choose_effort(terminal, &efforts).await? else {
        return Ok(None);
    };
    Ok(Some(Route {
        provider: ProviderRecord::parse(&ProviderKind::custom_profile(profile.name()))
            .ok_or("invalid custom Provider")?,
        model: profile.model().into(),
        effort,
        account_id: None,
    }))
}

async fn choose_effort(
    terminal: &mut AttachedTerminal,
    efforts: &[Option<Effort>],
) -> Result<Option<Option<Effort>>, SetupError> {
    let choices = efforts
        .iter()
        .enumerate()
        .map(|(index, effort)| {
            (
                u8::try_from(index + 1).unwrap_or(u8::MAX),
                effort.map_or("Default".into(), |effort| effort.to_string()),
            )
        })
        .collect::<Vec<_>>();
    let rows = choices
        .iter()
        .map(|(key, label)| (*key, label.as_str()))
        .collect::<Vec<_>>();
    let selected = choose(
        terminal,
        1,
        "Role effort",
        "Choose an effort allowed by this model or exact declaration",
        &rows,
    )
    .await?;
    Ok(selected.and_then(|key| efforts.get(usize::from(key.saturating_sub(1))).copied()))
}

fn tuple_label(route: &Route) -> String {
    let source = route
        .account_id
        .map_or_else(|| "environment".into(), |id| format!("account {id}"));
    format!(
        "{}/{} · {} · {source}",
        escape_terminal(&route.provider.profile()),
        escape_terminal(&route.model),
        arany::resolve_native_effort_for_run(&route.provider.profile(), &route.model, route.effort)
            .ok()
            .or(route.effort)
            .map_or("default".into(), |effort| effort.to_string())
    )
}

async fn manifest(
    state: &StateRoot,
    draft: &RoutingDraft,
    primary: Option<&Route>,
) -> Result<String, SetupError> {
    let (_, profiles) =
        CustomProfile::load_all_with_snapshot(state).map_err(|error| error.to_string())?;
    let statuses = CustomProfile::check_statuses(state, &profiles)
        .await
        .map_err(|error| error.to_string())?;
    let primary_chain = draft
        .providers
        .primary
        .clone()
        .unwrap_or_else(|| primary.cloned().into_iter().collect());
    let mut text = "Private role routing; every candidate is re-admitted before Workspace disclosure. No check or inference on Save.\n".to_owned();
    for (role, label) in ROLES {
        let configured = draft.providers.get(role);
        text.push_str(&format!(
            "\n{label}: {}\n",
            if configured.is_some() {
                "explicit"
            } else if role == ProviderRole::Primary {
                "current /model selection"
            } else {
                "Inherit primary"
            }
        ));
        let chain = configured.as_ref().unwrap_or(&primary_chain);
        if chain.is_empty() {
            text.push_str("No primary tuple selected; use /model before submission\n");
        }
        for (index, route) in chain.iter().enumerate() {
            text.push_str(&format!("  {}: {}\n", index + 1, tuple_label(route)));
            if let Some(name) = ProviderKind::custom_name(&route.provider.profile()) {
                if let Some(profile) = profiles.iter().find(|profile| profile.name() == name) {
                    text.push_str(&format!(
                        "    {} · Credential: {} — not checked by listing · {}\n",
                        escape_terminal(&profile.check_destination()),
                        escape_terminal(profile.credential_env()),
                        super::custom_status_label(&statuses, name)
                    ));
                } else {
                    text.push_str("    Declaration unavailable\n");
                }
            }
            if text.len() > MAX_ROLE_MANIFEST_BYTES {
                return Err("Resolved role manifest exceeds its display limit".into());
            }
        }
    }
    Ok(text)
}

async fn publish(
    journal: &mut SetupJournal,
    state: &StateRoot,
    draft: &RoutingDraft,
    bytes: Option<&[u8]>,
) -> Result<(), SetupError> {
    let operation = journal
        .start(
            ProviderSetupTargetKind::RoleRouting,
            "roles".into(),
            draft.expected().map(|value| Sha256::digest(value).into()),
        )
        .await?;
    match draft.publish(state, bytes) {
        Ok(()) => {
            journal
                .finish(
                    operation,
                    Some(Sha256::digest(bytes.unwrap_or_default()).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
        }
        Err(error) => {
            let uncertain = matches!(error, StoreError::ReplaceUnconfirmed);
            journal
                .finish(
                    operation,
                    None,
                    if uncertain {
                        ProviderSetupDisposition::Uncertain
                    } else {
                        ProviderSetupDisposition::Failed
                    },
                )
                .await?;
            if uncertain {
                Err(SetupError::Uncertain("Role routing publication is unknown; inspect saved state and start a new Session; no retry was attempted".into()))
            } else {
                Err(error.to_string().into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routing_edits_journal_save_stale_failure_and_clear_without_a_run() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).unwrap();
        let draft = RoutingDraft::load(&state).unwrap();
        let bytes = draft.encode(None).unwrap();
        let mut journal = SetupJournal::new(&path, &workspace, None);
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
        let current = RoutingDraft::load(&state).unwrap();
        assert!(publish(&mut journal, &state, &current, None).await.is_ok());
        let store = arany::Store::open_read_only(StateRoot::open_existing(&path).unwrap()).unwrap();
        let events = store
            .load_session(journal.session_id().unwrap())
            .await
            .unwrap();
        let view = store
            .load_view(journal.session_id().unwrap())
            .await
            .unwrap()
            .unwrap();
        store.close().await.unwrap();
        assert_eq!(events.len(), 7);
        assert!(view.runs.is_empty());
        assert!(view.pending_provider_setup_edit.is_none());
        assert!(!view.provider_setup_blocks_continuation());
        let dispositions = events
            .iter()
            .filter_map(|event| match event.event {
                arany::Event::ProviderSetupEditFinished { disposition, .. } => Some(disposition),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            dispositions,
            [
                ProviderSetupDisposition::Applied,
                ProviderSetupDisposition::Failed,
                ProviderSetupDisposition::Applied
            ]
        );
        for event in events.iter().filter_map(|event| match &event.event {
            arany::Event::ProviderSetupEditStarted { target, .. } => Some(target),
            _ => None,
        }) {
            assert_eq!(event.kind, ProviderSetupTargetKind::RoleRouting);
        }
        assert!(RoutingDraft::load(&state).unwrap().expected().is_none());
    }
}
