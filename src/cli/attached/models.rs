use super::{Admission, controls};
use crate::cli::{chatgpt, credentials};
use arany::{
    AttachedTerminal, Composer, ComposerEdit, CustomProfile, Effort, ModelCatalogState, ModelEntry,
    ModelPicker, NativeApiCredentials, SessionId, SessionView, ShutdownSignal, StateRoot,
    TerminalInput, list_native_models_with_credentials, resolve_native_effort,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::Path,
    pin::Pin,
};

const CATALOG_LOADING: &str = "Loading model catalog; Ctrl+C cancels";
const MAX_PREFERENCE_BYTES: usize = StateRoot::MAX_MODEL_PREFERENCES_BYTES;
const MAX_PREFERENCE_SOURCES: usize = 8;
const MAX_CACHED_MODELS: usize = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelPreferences {
    version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected: Option<SavedSelection>,
    #[serde(deserialize_with = "bounded_sources")]
    sources: Vec<SavedModels>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedSelection {
    profile: String,
    account_id: uuid::Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedModels {
    profile: String,
    account_id: uuid::Uuid,
    model: Option<String>,
    effort: Option<Effort>,
    #[serde(deserialize_with = "bounded_models")]
    catalog: Vec<String>,
}

fn bounded_sources<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SavedModels>, D::Error> {
    bounded_list::<D, SavedModels, MAX_PREFERENCE_SOURCES>(deserializer)
}

fn bounded_models<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    bounded_list::<D, String, MAX_CACHED_MODELS>(deserializer)
}

fn bounded_list<'de, D, T, const LIMIT: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> serde::de::Visitor<'de> for Visitor<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {LIMIT} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while values.len() < LIMIT {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "saved model preference limit exceeded",
                ));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Visitor::<T, LIMIT>(std::marker::PhantomData))
}

fn read_preferences(root: &StateRoot) -> Result<ModelPreferences, String> {
    let Some(record) = root
        .read_model_preferences_record()
        .map_err(|_| "saved model preferences unavailable or unsafe")?
    else {
        return Ok(ModelPreferences {
            version: 1,
            selected: None,
            sources: Vec::new(),
        });
    };
    let preferences: ModelPreferences =
        serde_json::from_slice(&record).map_err(|_| "invalid saved model preferences")?;
    validate_preferences(preferences)
}

fn validate_preferences(preferences: ModelPreferences) -> Result<ModelPreferences, String> {
    let mut sources = HashSet::new();
    if preferences.version != 1 || preferences.sources.len() > MAX_PREFERENCE_SOURCES {
        return Err("invalid saved model preferences".into());
    }
    for source in &preferences.sources {
        let mut models = HashSet::new();
        let valid_selection = match source.model.as_deref() {
            Some(model) if arany::validate_native_model_id(model).is_ok() => {
                if source.profile == "chatgpt" {
                    source.effort.is_some()
                } else {
                    arany::resolve_native_effort_for_run(&source.profile, model, source.effort)
                        .is_ok()
                }
            }
            None => source.effort.is_none(),
            _ => false,
        };
        if !matches!(source.profile.as_str(), "openai" | "anthropic" | "chatgpt")
            || source.account_id.get_version() != Some(uuid::Version::SortRand)
            || !sources.insert((&source.profile, source.account_id))
            || !valid_selection
            || source.catalog.len() > MAX_CACHED_MODELS
            || source.catalog.iter().any(|model| {
                arany::validate_native_model_id(model).is_err() || !models.insert(model)
            })
        {
            return Err("invalid saved model preferences".into());
        }
    }
    if preferences.selected.as_ref().is_some_and(|selected| {
        !preferences.sources.iter().any(|source| {
            source.profile == selected.profile && source.account_id == selected.account_id
        })
    }) {
        return Err("invalid saved model preferences".into());
    }
    Ok(preferences)
}

fn matching_source<'a>(
    preferences: &'a ModelPreferences,
    defaults: &arany::SessionDefaults,
) -> Option<&'a SavedModels> {
    preferences.sources.iter().find(|source| {
        Some(source.profile.as_str()) == defaults.provider.as_deref()
            && Some(source.account_id) == defaults.account_id
    })
}

pub(super) fn last_saved_defaults(
    workspace: &Path,
) -> Result<Option<arany::SessionDefaults>, String> {
    let path = StateRoot::account_path().map_err(|_| "saved selection unavailable")?;
    let root = match StateRoot::open_existing(&path) {
        Ok(root) => root,
        Err(arany::StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            if matches!(std::fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(None);
            }
            return Err("saved selection unavailable or unsafe".into());
        }
        Err(_) => return Err("saved selection unavailable or unsafe".into()),
    };
    root.with_account_replacement_lock(workspace, || {
        let preferences = read_preferences(&root)?;
        Ok(preferences
            .selected
            .as_ref()
            .and_then(|selected| {
                preferences.sources.iter().find(|source| {
                    source.profile == selected.profile && source.account_id == selected.account_id
                })
            })
            .map(|source| arany::SessionDefaults {
                provider: Some(source.profile.clone()),
                model: source.model.clone(),
                effort: source.effort,
                account_id: Some(source.account_id),
                ..arany::SessionDefaults::default()
            }))
    })
    .map_err(|_| "saved selection unavailable".to_owned())?
}

pub(super) async fn restore_saved_models(
    workspace: &Path,
    state_dir: &Path,
    defaults: &mut arany::SessionDefaults,
    restore_selection: bool,
) -> Result<Vec<ModelEntry>, String> {
    if defaults.account_id.is_none() {
        return Ok(Vec::new());
    }
    let root = StateRoot::open_existing(
        &StateRoot::account_path().map_err(|_| "saved model preferences unavailable")?,
    )
    .map_err(|_| "saved model preferences unavailable")?;
    let saved = root
        .with_account_replacement_lock(workspace, || {
            let preferences = read_preferences(&root)?;
            Ok::<_, String>(matching_source(&preferences, defaults).cloned())
        })
        .map_err(|_| "saved model preferences unavailable")??;
    if let Some(source) = saved {
        if restore_selection && source.model.is_some() {
            defaults.model = source.model.clone();
            defaults.effort = source.effort;
        }
        Ok(source
            .catalog
            .iter()
            .map(|id| ModelEntry {
                id: id.clone(),
                runnable: resolve_native_effort(&source.profile, id, None).is_ok(),
                efforts: Effort::ALL
                    .into_iter()
                    .filter(|effort| {
                        resolve_native_effort(&source.profile, id, Some(*effort)).is_ok()
                    })
                    .collect(),
            })
            .collect())
    } else {
        if restore_selection && let Ok(state) = StateRoot::open_existing(state_dir) {
            match arany::continue_session(state, workspace.to_path_buf()).await {
                Ok(previous)
                    if previous.defaults.provider == defaults.provider
                        && previous.defaults.account_id == defaults.account_id
                        && previous.defaults.model.is_some() =>
                {
                    defaults.model = previous.defaults.model;
                    defaults.effort = previous.defaults.effort;
                    remember_models(workspace, defaults, None, true).await?;
                }
                Ok(_) | Err(arany::EngineError::NoSessionForWorkspace) => {}
                Err(_) => return Err("previous model selection unavailable; choose /model".into()),
            }
        }
        Ok(Vec::new())
    }
}

pub(super) async fn remember_models(
    workspace: &Path,
    defaults: &arany::SessionDefaults,
    catalog: Option<&[ModelEntry]>,
    remember_selection: bool,
) -> Result<(), String> {
    let Some(account_id) = defaults.account_id else {
        return Ok(());
    };
    let Some(profile) = defaults
        .provider
        .as_deref()
        .filter(|profile| matches!(*profile, "openai" | "anthropic" | "chatgpt"))
    else {
        return Ok(());
    };
    let source = SavedModels {
        profile: profile.to_owned(),
        account_id,
        model: defaults.model.clone(),
        effort: defaults.effort,
        catalog: catalog
            .unwrap_or_default()
            .iter()
            .map(|item| item.id.clone())
            .collect(),
    };
    let replace_catalog = catalog.is_some();
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let root = StateRoot::open_existing(
            &StateRoot::account_path().map_err(|_| "model preferences unavailable")?,
        )
        .map_err(|_| "model preferences unavailable")?;
        root.with_account_replacement_lock(&workspace, || {
            write_preferences(&root, source, replace_catalog, remember_selection)
        })
        .map_err(|_| "model preferences unavailable")?
    })
    .await
    .map_err(|_| "model preferences unavailable")?
}

fn write_preferences(
    root: &StateRoot,
    source: SavedModels,
    replace_catalog: bool,
    remember_selection: bool,
) -> Result<(), String> {
    let mut preferences = read_preferences(root)?;
    let mut source = source;
    if let Some(index) = preferences
        .sources
        .iter()
        .position(|old| old.profile == source.profile && old.account_id == source.account_id)
    {
        let old = preferences.sources.remove(index);
        if !replace_catalog {
            source.catalog = old.catalog;
        }
        if source.model.is_none() || (!remember_selection && old.model.is_some()) {
            source.model = old.model;
            source.effort = old.effort;
        }
    }
    if remember_selection {
        preferences.selected = Some(SavedSelection {
            profile: source.profile.clone(),
            account_id: source.account_id,
        });
    }
    preferences.sources.push(source);
    while preferences.sources.len() > MAX_PREFERENCE_SOURCES {
        let index = preferences
            .sources
            .iter()
            .position(|source| {
                preferences.selected.as_ref().is_none_or(|selected| {
                    source.profile != selected.profile || source.account_id != selected.account_id
                })
            })
            .unwrap_or(0);
        preferences.sources.remove(index);
    }
    preferences = validate_preferences(preferences)?;
    loop {
        let record =
            serde_json::to_vec(&preferences).map_err(|_| "model preferences unavailable")?;
        if record.len() <= MAX_PREFERENCE_BYTES {
            root.replace_model_preferences_record(&record)
                .map_err(|_| "model preferences unavailable or unsafe")?;
            return Ok(());
        }
        if preferences.sources.len() == 1 {
            return Err("model catalog exceeds saved preference limit".into());
        }
        let index = preferences
            .sources
            .iter()
            .position(|source| {
                preferences.selected.as_ref().is_none_or(|selected| {
                    source.profile != selected.profile || source.account_id != selected.account_id
                })
            })
            .unwrap_or(0);
        preferences.sources.remove(index);
    }
}

pub(super) enum ModelBrowse {
    Notice(String),
    Shutdown(ShutdownSignal),
}

enum Choice {
    Selected {
        model: String,
        effort: Option<Effort>,
    },
    Closed,
    Shutdown(ShutdownSignal),
}

pub(super) async fn browse(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    composer: &mut Composer,
) -> Result<ModelBrowse, String> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Ok(ModelBrowse::Notice("Select /provider first".into()));
    };
    composer.set_completion_selection(
        Some(profile),
        view.defaults.model.as_deref(),
        view.defaults.account_id,
    );
    let cached = composer
        .cached_model_catalog()
        .filter(|(items, exact)| !items.is_empty() && !exact);
    let refreshing = cached.is_some();
    let (mut items, exact_custom) = match cached {
        Some(items) => items,
        None => match load_while_owned(terminal, admission, view, composer, profile).await? {
            Ok(items) => items,
            Err(outcome) => return Ok(outcome),
        },
    };
    composer.set_model_catalog(profile, &items, exact_custom);
    if items.is_empty() {
        return Ok(ModelBrowse::Notice(
            "No models available for the selected Provider".into(),
        ));
    }
    let selection = {
        let refresh = load_catalog(admission, view, profile);
        tokio::pin!(refresh);
        if terminal.is_linear() {
            terminal
                .restore_draft_input(0)
                .map_err(|error| error.to_string())?;
            browse_linear(
                terminal,
                &items,
                view,
                composer,
                exact_custom,
                refresh.as_mut(),
                refreshing,
            )
            .await
        } else {
            browse_inline(
                terminal,
                &mut items,
                view,
                composer,
                exact_custom,
                refresh.as_mut(),
                refreshing,
            )
            .await
        }
    };
    terminal.close_models();
    terminal
        .restore_draft_input(composer.text().len())
        .map_err(|error| error.to_string())?;
    match selection? {
        Choice::Closed => Ok(ModelBrowse::Notice("Model catalog closed".into())),
        Choice::Shutdown(signal) => Ok(ModelBrowse::Shutdown(signal)),
        Choice::Selected { model, effort } => {
            controls::select_model(admission, session_id, view, &model, effort)
                .await
                .map(ModelBrowse::Notice)
        }
    }
}

fn effort_choices(profile: &str, item: &ModelEntry, exact_custom: bool) -> Vec<Option<Effort>> {
    if exact_custom {
        return std::iter::once(None)
            .chain(item.efforts.iter().copied().map(Some))
            .collect();
    }
    let reviewed = resolve_native_effort(profile, &item.id, None).is_ok();
    reviewed
        .then_some(None)
        .into_iter()
        .chain(
            Effort::ALL
                .into_iter()
                .filter(|effort| {
                    !reviewed || resolve_native_effort(profile, &item.id, Some(*effort)).is_ok()
                })
                .map(Some),
        )
        .collect()
}

fn initial_efforts(
    items: &[ModelEntry],
    view: &SessionView,
    exact_custom: bool,
) -> Vec<Option<Effort>> {
    let profile = view.defaults.provider.as_deref().unwrap_or("");
    items
        .iter()
        .map(|item| {
            let choices = effort_choices(profile, item, exact_custom);
            if view.defaults.model.as_deref() == Some(&item.id)
                && choices.contains(&view.defaults.effort)
            {
                view.defaults.effort
            } else if choices.contains(&Some(Effort::Low)) {
                Some(Effort::Low)
            } else {
                choices[0]
            }
        })
        .collect()
}

async fn load_catalog(
    admission: &Admission,
    view: &SessionView,
    profile: &str,
) -> Result<(Vec<ModelEntry>, bool), String> {
    load_catalog_inner(admission, view, profile)
        .await
        .inspect_err(|_| {
            arany::record_development_failure(arany::DevelopmentFailure::ModelCatalog);
        })
}

async fn load_catalog_inner(
    admission: &Admission,
    view: &SessionView,
    profile: &str,
) -> Result<(Vec<ModelEntry>, bool), String> {
    let result = match profile {
        "openai" | "anthropic" => {
            let key = match view.defaults.account_id {
                Some(id) => credentials::load_selected(&admission.workspace, id, profile)
                    .await
                    .map_err(|error| error.to_string())?,
                None => {
                    NativeApiCredentials::from_env(profile).map_err(|error| error.to_string())?
                }
            };
            list_native_models_with_credentials(profile, &key)
                .await
                .map(|items| (items, false))
                .map_err(|error| error.to_string())
        }
        "chatgpt" => {
            let expected = view
                .defaults
                .account_id
                .ok_or_else(|| "select a ChatGPT account with /setup first".to_owned())?;
            let (selected, rows) =
                chatgpt::selected_models(admission.workspace.clone(), Some(expected))
                    .await
                    .map_err(|error| error.to_string())?;
            if selected != expected {
                return Err("selected ChatGPT account changed; use /setup".into());
            }
            Ok((
                rows.into_iter()
                    .map(|row| ModelEntry {
                        id: row.slug,
                        runnable: false,
                        efforts: Vec::new(),
                    })
                    .collect(),
                false,
            ))
        }
        _ => {
            let name = profile
                .strip_prefix("custom:")
                .ok_or_else(|| "invalid Provider profile".to_owned())?;
            let root = StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let custom =
                CustomProfile::load_named(&root, name).map_err(|error| error.to_string())?;
            let mut entry = ModelEntry::exact_custom(custom.model().to_owned());
            entry.efforts = custom.efforts().to_vec();
            Ok((vec![entry], true))
        }
    };
    if let Ok((items, false)) = &result {
        remember_models(&admission.workspace, &view.defaults, Some(items), false).await?;
    }
    result
}

async fn load_while_owned(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
    composer: &mut Composer,
    profile: &str,
) -> Result<Result<(Vec<ModelEntry>, bool), ModelBrowse>, String> {
    terminal
        .draw_busy(view, composer, Some(CATALOG_LOADING))
        .map_err(|error| error.to_string())?;
    let query = load_catalog(admission, view, profile);
    tokio::pin!(query);
    let outcome = loop {
        let at_input_boundary = terminal.input_boundary_ready();
        tokio::select! {
            result = &mut query, if at_input_boundary => break result.map_err(|error| {
                ModelBrowse::Notice(format!("Error: Model catalog unavailable: {error}. Review the selected account or retry /model."))
            }),
            input = terminal.next_input() => {
                match input.map_err(|error| error.to_string())? {
                    TerminalInput::ClipboardPaste => {
                        let notice = match terminal.request_clipboard_paste() {
                            Ok(()) => "Reading clipboard... draft retained".to_owned(),
                            Err(error) => format!("Error: {error}; draft unchanged; catalog loading"),
                        };
                        terminal.draw_busy(view, composer, Some(&notice)).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::Paste => {
                        let notice = match terminal.paste_into(composer) {
                            Ok(_) => CATALOG_LOADING.to_owned(),
                            Err(error) => format!("Error: {error}; draft unchanged; catalog loading"),
                        };
                        terminal.draw_busy(view, composer, Some(&notice)).map_err(|error| error.to_string())?;
                    }
                    input @ (TerminalInput::Character(_)
                    | TerminalInput::Newline
                    | TerminalInput::Backspace
                    | TerminalInput::Delete
                    | TerminalInput::Left
                    | TerminalInput::Right
                    | TerminalInput::WordLeft
                    | TerminalInput::WordRight
                    | TerminalInput::BackspaceWord
                    | TerminalInput::Up
                    | TerminalInput::Down
                    | TerminalInput::Home
                    | TerminalInput::End) => {
                        let notice = if composer.apply(input) == ComposerEdit::AtCapacity {
                            Some("Error: message is too long; shorten it; draft unchanged; catalog loading")
                        } else if terminal.is_linear() {
                            None
                        } else {
                            Some(CATALOG_LOADING)
                        };
                        terminal.draw_busy(view, composer, notice).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::Interrupt | TerminalInput::EndOfInput | TerminalInput::Escape => {
                        if !terminal.input_boundary_ready() {
                            terminal.discard_draft_input();
                        }
                        break Err(ModelBrowse::Notice("Model catalog cancelled".into()));
                    }
                    TerminalInput::Shutdown(signal) => break Err(ModelBrowse::Shutdown(signal)),
                    TerminalInput::Suspend => {
                        terminal.suspend_and_resume(composer.text().len()).map_err(|error| error.to_string())?;
                        terminal.draw_busy(view, composer, Some(CATALOG_LOADING)).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::LineRejected => {
                        terminal.draw_busy(view, composer, Some("Error: invalid or overlong terminal line; draft unchanged; catalog loading")).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::LineContinued => {
                        terminal.draw_busy(view, composer, Some(&format!(
                            "Draft: {} characters; catalog loading; Ctrl+C cancels",
                            composer.character_count()
                        ))).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::Submit => {
                        terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
                        terminal.draw_busy(view, composer, Some(CATALOG_LOADING)).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::Resize => {
                        terminal.draw_busy(view, composer, Some(CATALOG_LOADING)).map_err(|error| error.to_string())?;
                    }
                    _ => {}
                }
            }
        }
    };
    terminal
        .restore_draft_input(composer.text().len())
        .map_err(|error| error.to_string())?;
    Ok(outcome)
}

async fn browse_inline(
    terminal: &mut AttachedTerminal,
    items: &mut Vec<ModelEntry>,
    view: &SessionView,
    composer: &mut Composer,
    exact_custom: bool,
    mut refresh: Pin<&mut impl Future<Output = Result<(Vec<ModelEntry>, bool), String>>>,
    refreshing: bool,
) -> Result<Choice, String> {
    let profile = view.defaults.provider.as_deref().unwrap_or("");
    let mut anchor = view
        .defaults
        .model
        .as_deref()
        .filter(|id| items.iter().any(|item| item.id == *id))
        .unwrap_or(&items[0].id)
        .to_owned();
    let mut efforts = initial_efforts(items, view, exact_custom);
    let mut catalog_state = if refreshing {
        ModelCatalogState::Refreshing
    } else {
        ModelCatalogState::Current
    };
    let mut filter = String::new();
    let (mut visible, mut selected) = matching_models(items, &filter, &anchor);
    loop {
        let rows = visible
            .iter()
            .map(|index| items[*index].clone())
            .collect::<Vec<_>>();
        let levels = visible
            .iter()
            .map(|index| efforts[*index])
            .collect::<Vec<_>>();
        let ready = terminal
            .draw_models(
                view,
                composer,
                &ModelPicker {
                    items: &rows,
                    efforts: &levels,
                    selected,
                    exact_custom,
                    filter: &filter,
                    invalid: false,
                    catalog_state,
                },
            )
            .map_err(|error| error.to_string())?;
        let at_input_boundary = terminal.input_boundary_ready();
        let input = tokio::select! {
            result = &mut refresh, if catalog_state == ModelCatalogState::Refreshing && at_input_boundary => {
                match result {
                    Ok((updated, false)) => {
                        let mut updated_efforts = initial_efforts(&updated, view, false);
                        let previous = items.iter().zip(&efforts).map(|(item, effort)| (item.id.as_str(), *effort)).collect::<HashMap<_, _>>();
                        for (index, item) in updated.iter().enumerate() {
                            if let Some(effort) = previous.get(item.id.as_str())
                                && effort_choices(profile, item, false).contains(effort) {
                                updated_efforts[index] = *effort;
                            }
                        }
                        *items = updated;
                        efforts = updated_efforts;
                        composer.set_model_catalog(profile, items, false);
                        (visible, selected) = matching_models(items, &filter, &anchor);
                        catalog_state = ModelCatalogState::Updated;
                    }
                    _ => catalog_state = ModelCatalogState::RefreshFailed,
                }
                continue;
            }
            input = terminal.next_input() => input.map_err(|error| error.to_string())?,
        };
        if !ready
            && !matches!(
                input,
                TerminalInput::Resize
                    | TerminalInput::Escape
                    | TerminalInput::Interrupt
                    | TerminalInput::EndOfInput
                    | TerminalInput::Shutdown(_)
                    | TerminalInput::Suspend
            )
        {
            continue;
        }
        match input {
            TerminalInput::Character(character)
                if character.is_ascii_graphic() && filter.len() < 64 =>
            {
                filter.push(character);
                (visible, selected) = matching_models(items, &filter, &anchor);
            }
            TerminalInput::Backspace => {
                if filter.pop().is_some() {
                    (visible, selected) = matching_models(items, &filter, &anchor);
                }
            }
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down => selected = (selected + 1).min(visible.len().saturating_sub(1)),
            TerminalInput::PageUp => selected = selected.saturating_sub(10),
            TerminalInput::PageDown => {
                selected = (selected + 10).min(visible.len().saturating_sub(1))
            }
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = visible.len().saturating_sub(1),
            direction @ (TerminalInput::Left | TerminalInput::Right) => {
                if let Some(index) = visible.get(selected) {
                    let choices = effort_choices(profile, &items[*index], exact_custom);
                    let current = choices
                        .iter()
                        .position(|effort| *effort == efforts[*index])
                        .unwrap_or(0);
                    let next = if direction == TerminalInput::Left {
                        current.saturating_sub(1)
                    } else {
                        (current + 1).min(choices.len() - 1)
                    };
                    efforts[*index] = choices[next];
                }
            }
            TerminalInput::Submit => {
                if let Some(index) = visible.get(selected) {
                    return Ok(Choice::Selected {
                        model: items[*index].id.clone(),
                        effort: efforts[*index],
                    });
                }
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(composer.text().len())
                .map_err(|error| error.to_string())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(Choice::Closed);
            }
            TerminalInput::Shutdown(signal) => return Ok(Choice::Shutdown(signal)),
            _ => {}
        }
        if let Some(index) = visible.get(selected) {
            anchor.clone_from(&items[*index].id);
        }
    }
}

fn matching_models(items: &[ModelEntry], filter: &str, anchor: &str) -> (Vec<usize>, usize) {
    let visible = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            filter.is_empty()
                || item
                    .id
                    .as_bytes()
                    .windows(filter.len())
                    .any(|part| part.eq_ignore_ascii_case(filter.as_bytes()))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let selected = visible
        .iter()
        .position(|index| items[*index].id == anchor)
        .unwrap_or(0);
    (visible, selected)
}

async fn browse_linear(
    terminal: &mut AttachedTerminal,
    items: &[ModelEntry],
    view: &SessionView,
    composer: &mut Composer,
    exact_custom: bool,
    mut refresh: Pin<&mut impl Future<Output = Result<(Vec<ModelEntry>, bool), String>>>,
    refreshing: bool,
) -> Result<Choice, String> {
    let profile = view.defaults.provider.as_deref().unwrap_or("");
    let mut start = view
        .defaults
        .model
        .as_deref()
        .and_then(|id| items.iter().position(|item| item.id == id))
        .unwrap_or(0)
        / 10
        * 10;
    let efforts = initial_efforts(items, view, exact_custom);
    let mut input = String::new();
    let mut invalid = false;
    let mut too_long = false;
    let mut catalog_state = if refreshing {
        ModelCatalogState::Refreshing
    } else {
        ModelCatalogState::Current
    };
    loop {
        terminal
            .draw_models(
                view,
                composer,
                &ModelPicker {
                    items,
                    efforts: &efforts,
                    selected: start,
                    exact_custom,
                    filter: "",
                    invalid,
                    catalog_state,
                },
            )
            .map_err(|error| error.to_string())?;
        invalid = false;
        let at_input_boundary = terminal.input_boundary_ready();
        let next = tokio::select! {
            result = &mut refresh, if catalog_state == ModelCatalogState::Refreshing && at_input_boundary => {
                match result {
                    Ok((updated, false)) => {
                        composer.set_model_catalog(profile, &updated, false);
                        catalog_state = ModelCatalogState::Updated;
                    }
                    _ => catalog_state = ModelCatalogState::RefreshFailed,
                }
                continue;
            }
            input = terminal.next_input() => input.map_err(|error| error.to_string())?,
        };
        match next {
            TerminalInput::Character(character)
                if (character.is_ascii_graphic() || character == ' ')
                    && input.len() < 32
                    && !too_long =>
            {
                input.push(character)
            }
            TerminalInput::Character(_) => too_long = true,
            TerminalInput::Backspace => {
                input.pop();
            }
            TerminalInput::Submit => {
                let choice = input.trim();
                match choice {
                    "q" if !too_long => return Ok(Choice::Closed),
                    "n" if !too_long && start + 10 < items.len() => start += 10,
                    "p" if !too_long && start > 0 => start = start.saturating_sub(10),
                    _ => {
                        let mut parts = choice.split_ascii_whitespace();
                        if !too_long
                            && let Some(number) =
                                parts.next().and_then(|value| value.parse::<usize>().ok())
                            && (1..=10).contains(&number)
                            && let Some(item) = items.get(start + number - 1)
                        {
                            let level = match parts.next() {
                                None => Some(efforts[start + number - 1]),
                                Some("default") => Some(None),
                                Some(value) => value.parse::<Effort>().ok().map(Some),
                            };
                            if parts.next().is_none()
                                && let Some(effort) = level
                                && effort_choices(profile, item, exact_custom).contains(&effort)
                            {
                                return Ok(Choice::Selected {
                                    model: item.id.clone(),
                                    effort,
                                });
                            }
                        }
                        invalid = true;
                    }
                }
                input.clear();
                too_long = false;
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                invalid = true;
                input.clear();
                too_long = false;
                terminal.discard_draft_input();
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(0)
                .map_err(|error| error.to_string())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(Choice::Closed);
            }
            TerminalInput::Shutdown(signal) => return Ok(Choice::Shutdown(signal)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_filter_preserves_identity_and_can_recover_from_no_matches() {
        let items = [
            ModelEntry::exact_custom("gpt-6-astra".into()),
            ModelEntry::exact_custom("gpt-6-luna".into()),
            ModelEntry::exact_custom("gpt-6.1-sol".into()),
        ];
        let (visible, selected) = matching_models(&items, "LUNA", "gpt-6-luna");
        assert_eq!(visible.len(), 1);
        assert_eq!(items[visible[selected]].id, "gpt-6-luna");

        let (visible, selected) = matching_models(&items, "absent", "gpt-6-luna");
        assert!(visible.is_empty());
        assert_eq!(selected, 0);

        let (visible, selected) = matching_models(&items, "gpt-6", "gpt-6-luna");
        assert_eq!(visible.len(), 3);
        assert_eq!(items[visible[selected]].id, "gpt-6-luna");
    }
}

#[cfg(test)]
mod preference_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_preferences_reopen_only_bounded_valid_account_selections() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let id = uuid::Uuid::now_v7();
        let source = json!({
            "profile": "chatgpt", "account_id": id, "model": "gpt-6.1-sol",
            "effort": "medium", "catalog": ["gpt-6.1-sol", "another-model"]
        });
        let document = json!({"version": 1, "sources": [source.clone()]});
        let mut cases = vec![(document.clone(), true)];
        for (profile, account_id, accepted) in [
            ("chatgpt", id, true),
            ("openai", id, false),
            ("chatgpt", uuid::Uuid::now_v7(), false),
        ] {
            let mut changed = document.clone();
            changed["selected"] = json!({"profile": profile, "account_id": account_id});
            cases.push((changed, accepted));
        }
        let mut absent_model = document.clone();
        absent_model["selected"] = json!({"profile": "chatgpt", "account_id": id});
        absent_model["sources"][0]["model"] = json!(null);
        absent_model["sources"][0]["effort"] = json!(null);
        cases.push((absent_model, true));
        for (field, value) in [
            ("profile", json!("custom:host")),
            ("account_id", json!("2e664d00-3f9a-40c3-adeb-c6447313a871")),
            ("effort", json!(null)),
            ("model", json!("bad\nmodel")),
            ("catalog", json!(["duplicate", "duplicate"])),
            ("catalog", json!(["bad\u{1b}model"])),
            ("runnable", json!(true)),
        ] {
            let mut changed = document.clone();
            changed["sources"][0][field] = value;
            cases.push((changed, false));
        }
        let mut changed = document.clone();
        changed["version"] = json!(2);
        cases.push((changed, false));
        let mut changed = document.clone();
        changed["sources"] = json!([source.clone(), source.clone()]);
        cases.push((changed, false));
        let mut changed = document.clone();
        changed["sources"] = json!(vec![source.clone(); MAX_PREFERENCE_SOURCES + 1]);
        cases.push((changed, false));
        for count in [MAX_CACHED_MODELS, MAX_CACHED_MODELS + 1] {
            let mut changed = document.clone();
            changed["sources"][0]["catalog"] = json!(
                (0..count)
                    .map(|index| format!("model-{index}"))
                    .collect::<Vec<_>>()
            );
            cases.push((changed, count == MAX_CACHED_MODELS));
        }
        for (document, accepted) in cases {
            root.replace_model_preferences_record(&serde_json::to_vec(&document).unwrap())
                .unwrap();
            let reopened = StateRoot::open_existing(root.path()).unwrap();
            assert_eq!(read_preferences(&reopened).is_ok(), accepted);
        }
        root.replace_model_preferences_record(&serde_json::to_vec(&document).unwrap())
            .unwrap();
        let preferences = read_preferences(&root).unwrap();
        let mut defaults = arany::SessionDefaults {
            provider: Some("chatgpt".into()),
            account_id: Some(id),
            ..arany::SessionDefaults::default()
        };
        assert_eq!(
            matching_source(&preferences, &defaults).unwrap().effort,
            Some(Effort::Medium)
        );
        defaults.account_id = Some(uuid::Uuid::now_v7());
        assert!(matching_source(&preferences, &defaults).is_none());
        defaults.account_id = Some(id);
        defaults.provider = Some("openai".into());
        assert!(matching_source(&preferences, &defaults).is_none());
        assert!(
            root.replace_model_preferences_record(&vec![b'x'; MAX_PREFERENCE_BYTES + 1])
                .is_err()
        );
        assert_eq!(
            root.read_model_preferences_record().unwrap().unwrap(),
            serde_json::to_vec(&document).unwrap()
        );
        let original: SavedModels = serde_json::from_value(source).unwrap();
        let mut choice = original.clone();
        choice.effort = Some(Effort::High);
        choice.catalog.clear();
        write_preferences(&root, choice, false, true).unwrap();
        let mut stale_refresh = original.clone();
        stale_refresh.catalog = vec!["fresh-model".into()];
        write_preferences(&root, stale_refresh, true, false).unwrap();
        let reopened = StateRoot::open_existing(root.path()).unwrap();
        let current = read_preferences(&reopened).unwrap();
        assert_eq!(current.sources[0].effort, Some(Effort::High));
        assert_eq!(current.sources[0].catalog, ["fresh-model"]);
        assert_eq!(current.selected.as_ref().unwrap().account_id, id);
        let mut other_refresh = original.clone();
        other_refresh.account_id = uuid::Uuid::now_v7();
        write_preferences(&root, other_refresh, true, false).unwrap();
        assert_eq!(
            read_preferences(&root)
                .unwrap()
                .selected
                .unwrap()
                .account_id,
            id,
            "catalog completion cannot choose the billing source"
        );
        for _ in 0..MAX_PREFERENCE_SOURCES {
            let mut other = original.clone();
            other.account_id = uuid::Uuid::now_v7();
            write_preferences(&root, other, true, true).unwrap();
        }
        let current = read_preferences(&reopened).unwrap();
        assert_eq!(current.sources.len(), MAX_PREFERENCE_SOURCES);
        assert!(current.sources.iter().all(|source| source.account_id != id));
        let mut large = original;
        large.catalog = (0..MAX_CACHED_MODELS)
            .map(|index| format!("{index:04}{}", "x".repeat(124)))
            .collect();
        write_preferences(&root, large.clone(), true, true).unwrap();
        large.account_id = uuid::Uuid::now_v7();
        write_preferences(&root, large.clone(), true, true).unwrap();
        let current = read_preferences(&reopened).unwrap();
        assert_eq!(current.sources.len(), 1, "byte quota evicts older sources");
        assert_eq!(current.sources[0].account_id, large.account_id);
    }
}
