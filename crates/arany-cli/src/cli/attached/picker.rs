use super::{Admission, Selector};
use crate::terminal::{AttachedTerminal, Composer, ShutdownSignal, TerminalInput};
use arany::{SessionId, SessionListItem, SessionView, StateRoot, list_sessions};
use std::str::FromStr;

pub(super) enum PickerChoice {
    Selected(SessionId),
    Unavailable(String),
    Empty,
    Closed,
    Shutdown(ShutdownSignal),
}

pub(super) enum QuickChoice {
    Selected(Selector),
    Closed,
    Shutdown(ShutdownSignal),
}

pub(super) async fn pick_quick_action(
    terminal: &mut AttachedTerminal,
    composer: &Composer,
    view: &SessionView,
) -> Result<QuickChoice, String> {
    let action = |index| match index {
        0 => Selector::Models,
        1 => Selector::Agents,
        2 => Selector::Resume,
        _ => Selector::Setup,
    };
    let mut selected = if view.defaults.provider.is_none() {
        3
    } else {
        0
    };
    terminal.begin_dialog();
    loop {
        terminal
            .draw_quick_actions(selected, !composer.is_empty(), view)
            .map_err(|error| error.to_string())?;
        if terminal
            .arm_dialog()
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(index) = input.choice_index(4) {
            return Ok(QuickChoice::Selected(action(index)));
        }
        match input {
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down => selected = (selected + 1).min(3),
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = 3,
            TerminalInput::Submit => return Ok(QuickChoice::Selected(action(selected))),
            TerminalInput::Suspend => terminal
                .suspend_and_resume(composer.input_capacity())
                .map_err(|error| error.to_string())?,
            TerminalInput::Escape
            | TerminalInput::Interrupt
            | TerminalInput::EndOfInput
            | TerminalInput::QuickActions => return Ok(QuickChoice::Closed),
            TerminalInput::Shutdown(signal) => return Ok(QuickChoice::Shutdown(signal)),
            _ => {}
        }
    }
}

pub(super) async fn pick_session(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: Option<&SessionView>,
) -> Result<PickerChoice, String> {
    let fallback = view.map(|view| &view.defaults);
    let root = match StateRoot::open_existing(&admission.state_dir) {
        Ok(root) => root,
        Err(_) => {
            return Ok(PickerChoice::Unavailable(
                "state directory unavailable or unsafe".into(),
            ));
        }
    };
    let mut items = match list_sessions(root, admission.workspace.clone()).await {
        Ok(items) => items,
        Err(error) => return Ok(PickerChoice::Unavailable(error.to_string())),
    };
    if items.is_empty() {
        return Ok(PickerChoice::Empty);
    }
    if items.iter().any(|item| item.defaults.provider.is_none()) {
        let inherited = match fallback.filter(|defaults| defaults.provider.is_some()) {
            Some(defaults) => Some(defaults.clone()),
            None => match super::models::last_saved_defaults(&admission.workspace) {
                Ok(defaults) => defaults,
                Err(error) => return Ok(PickerChoice::Unavailable(error)),
            },
        };
        if let Some(defaults) = inherited {
            for item in &mut items {
                if item.defaults.provider.is_none() {
                    let policy = item.defaults.policy;
                    item.defaults = defaults.clone();
                    item.defaults.policy = policy;
                }
            }
        }
    }
    terminal.begin_dialog();
    let result = if terminal.is_linear() {
        pick_session_linear(terminal, &items).await
    } else {
        pick_session_inline(terminal, &items, view).await
    };
    terminal
        .close_sessions()
        .map_err(|error| error.to_string())?;
    result
}

async fn pick_session_inline(
    terminal: &mut AttachedTerminal,
    items: &[SessionListItem],
    view: Option<&SessionView>,
) -> Result<PickerChoice, String> {
    let mut selected = 0;
    loop {
        terminal
            .draw_sessions(items, selected, view)
            .map_err(|error| error.to_string())?;
        if terminal
            .arm_dialog()
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        match terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?
        {
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down => selected = (selected + 1).min(items.len() - 1),
            TerminalInput::PageUp => selected = selected.saturating_sub(10),
            TerminalInput::PageDown => selected = (selected + 10).min(items.len() - 1),
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = items.len() - 1,
            TerminalInput::PointerMove { column, row } => {
                if let Some(index) =
                    terminal.session_picker_target(column, row, selected, items.len())
                {
                    selected = index;
                }
            }
            TerminalInput::PointerClick { column, row, .. } => {
                if let Some(index) =
                    terminal.session_picker_target(column, row, selected, items.len())
                {
                    return Ok(PickerChoice::Selected(items[index].id));
                }
            }
            TerminalInput::PointerScrollUp { column, row } => {
                if terminal
                    .session_picker_target(column, row, selected, items.len())
                    .is_some()
                {
                    selected = selected.saturating_sub(1);
                }
            }
            TerminalInput::PointerScrollDown { column, row } => {
                if terminal
                    .session_picker_target(column, row, selected, items.len())
                    .is_some()
                {
                    selected = (selected + 1).min(items.len() - 1);
                }
            }
            TerminalInput::Submit => return Ok(PickerChoice::Selected(items[selected].id)),
            TerminalInput::Suspend => terminal
                .suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)
                .map_err(|error| error.to_string())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(PickerChoice::Closed);
            }
            TerminalInput::Shutdown(signal) => return Ok(PickerChoice::Shutdown(signal)),
            _ => {}
        }
    }
}

async fn pick_session_linear(
    terminal: &mut AttachedTerminal,
    items: &[SessionListItem],
) -> Result<PickerChoice, String> {
    let mut start = 0;
    let mut input = String::new();
    let mut too_long = false;
    loop {
        terminal
            .draw_sessions(items, start, None)
            .map_err(|error| error.to_string())?;
        if terminal
            .arm_dialog()
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        match terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?
        {
            TerminalInput::Character(character) => {
                if character.is_ascii_graphic() && input.len() < 64 {
                    input.push(character);
                } else if character != ' ' || input.len() >= 64 {
                    too_long = true;
                } else {
                    input.push(character);
                }
            }
            TerminalInput::Backspace => {
                input.pop();
            }
            TerminalInput::Submit => {
                let choice = input.trim();
                if !too_long {
                    match choice {
                        "q" => return Ok(PickerChoice::Closed),
                        "n" if start + 10 < items.len() => {
                            start += 10;
                            input.clear();
                            continue;
                        }
                        "p" if start > 0 => {
                            start -= 10;
                            input.clear();
                            continue;
                        }
                        _ => {}
                    }
                    if let Ok(number) = choice.parse::<usize>()
                        && (1..=10).contains(&number)
                        && let Some(item) = items.get(start + number - 1)
                    {
                        return Ok(PickerChoice::Selected(item.id));
                    }
                    if let Ok(id) = SessionId::from_str(choice)
                        && items.iter().any(|item| item.id == id)
                    {
                        return Ok(PickerChoice::Selected(id));
                    }
                }
                terminal
                    .session_picker_invalid()
                    .map_err(|error| error.to_string())?;
                input.clear();
                too_long = false;
                terminal.redraw_sessions();
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                terminal
                    .session_picker_invalid()
                    .map_err(|error| error.to_string())?;
                input.clear();
                too_long = false;
                terminal.discard_draft_input();
                terminal.redraw_sessions();
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)
                .map_err(|error| error.to_string())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(PickerChoice::Closed);
            }
            TerminalInput::Shutdown(signal) => return Ok(PickerChoice::Shutdown(signal)),
            _ => {}
        }
    }
}
