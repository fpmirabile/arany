use super::{Admission, controls};
use crate::cli::{chatgpt, credentials};
use arany::{
    AttachedTerminal, Composer, ComposerEdit, CustomProfile, Effort, ModelCatalogState, ModelEntry,
    ModelPicker, NativeApiCredentials, SessionId, SessionView, ShutdownSignal, StateRoot,
    TerminalInput, list_native_models_with_credentials, resolve_native_effort,
};
use std::{collections::HashMap, future::Future, pin::Pin};

const CATALOG_LOADING: &str = "Loading model catalog; Ctrl+C cancels";

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
    match profile {
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
    }
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
            result = &mut query, if at_input_boundary => break result.map_err(|error| ModelBrowse::Notice(format!(
                "Error: Model catalog unavailable: {error}. Review the selected account or retry /models."
            ))),
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
                            Some("Error: input exceeds 8 KiB; draft unchanged; catalog loading")
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
                            "Draft: {} of 8192 bytes; catalog loading; Ctrl+C cancels",
                            composer.text().len()
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
