use super::SetupError;
use crate::terminal::{AttachedTerminal, Composer, TerminalInput};
use std::future::Future;

const SETUP_MESSAGE_PAGE_CHARS: usize = 36;
const MAX_SETUP_CHOICE_INPUT_BYTES: usize = 64;

pub(in super::super) async fn review_warning(
    terminal: &mut AttachedTerminal,
    title: &str,
    warning: &str,
    instruction: &str,
) -> Result<bool, SetupError> {
    if terminal.is_linear() {
        return review_linear_warning(terminal, title, warning, instruction).await;
    }
    let mut page = 0usize;
    let mut accept_selected = false;
    let mut notice = None::<&str>;
    loop {
        let has_more =
            terminal.draw_setup_warning(title, warning, page, accept_selected, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if has_more => page += 1,
            TerminalInput::PageDown | TerminalInput::Down | TerminalInput::Tab if has_more => {
                page += 1;
            }
            TerminalInput::PageUp => {
                page = page.saturating_sub(1);
                accept_selected = false;
            }
            TerminalInput::Up if has_more => page = page.saturating_sub(1),
            TerminalInput::Up if !accept_selected && page > 0 => page -= 1,
            TerminalInput::Left | TerminalInput::Up if !has_more => accept_selected = false,
            TerminalInput::Right | TerminalInput::Down if !has_more => accept_selected = true,
            TerminalInput::Tab if !has_more => accept_selected = !accept_selected,
            TerminalInput::Character(_) => {
                notice = Some(if has_more {
                    "Read every page before accepting"
                } else {
                    "Tab chooses; Enter confirms"
                });
            }
            TerminalInput::Submit if accept_selected => {
                return Ok(true);
            }
            TerminalInput::Submit => {
                terminal.discard_draft_input();
                return Ok(false);
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                accept_selected = false;
                terminal.discard_draft_input();
                notice = Some("Tab chooses; Enter confirms");
            }
            TerminalInput::Resize => {
                page = 0;
                accept_selected = false;
                notice = Some("Resize: reread");
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                page = 0;
                accept_selected = false;
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => {
                return Err(SetupError::Shutdown(signal));
            }
            _ => {}
        }
    }
}

async fn review_linear_warning(
    terminal: &mut AttachedTerminal,
    title: &str,
    warning: &str,
    instruction: &str,
) -> Result<bool, SetupError> {
    let pages = setup_message_pages(warning);
    let mut page = 0usize;
    let mut notice = None;
    loop {
        let text = format!("Page {}/{}\n{}", page + 1, pages.len(), pages[page]);
        terminal.draw_setup(0, title, &text, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if page + 1 < pages.len() => page += 1,
            TerminalInput::Down | TerminalInput::PageDown if page + 1 < pages.len() => page += 1,
            TerminalInput::Submit => {
                if let Some(accepted) =
                    review_linear_confirmation(terminal, title, instruction).await?
                {
                    return Ok(accepted);
                }
                page = 0;
            }
            TerminalInput::Up | TerminalInput::PageUp => page = page.saturating_sub(1),
            TerminalInput::Character(_) => {
                notice = Some("Read every page before accepting");
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Resize => {
                page = 0;
                notice = Some("Resize: reread");
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                page = 0;
            }
            _ => {}
        }
    }
}

async fn review_linear_confirmation(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
) -> Result<Option<bool>, SetupError> {
    let choices = [(b'1', "Back"), (b'2', "Accept")];
    let linear_choices = linear_rendered_choices(&choices);
    let rendered = linear_choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let mut input = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(2, title, instruction, &rendered, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit => {
                let accepted = if rejected == 0 && input.trim().is_empty() {
                    Some(choices[0].0)
                } else if rejected == 0 {
                    linear_choice_value(input.trim(), &choices)
                } else {
                    None
                };
                terminal.discard_draft_input();
                if let Some(value) = accepted {
                    return Ok(Some(value == b'2'));
                }
                input.clear();
                rejected = 0;
                notice = Some("Type a listed choice name; Ctrl+C cancels");
            }
            TerminalInput::Character(character) => {
                if !append_linear_choice_character(&mut input, &mut rejected, character) {
                    notice = Some("Invalid choice input; use Backspace to correct");
                }
            }
            TerminalInput::Backspace => {
                if rejected > 0 {
                    rejected -= 1;
                } else {
                    input.pop();
                }
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                input.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Invalid choice line; Ctrl+C cancels");
            }
            TerminalInput::Resize => {
                terminal.discard_approval_typeahead().await?;
                return Ok(None);
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                terminal.discard_approval_typeahead().await?;
                return Ok(None);
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(Some(false));
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
}

pub(super) async fn await_setup_ack(
    terminal: &mut AttachedTerminal,
    title: &str,
    message: &str,
) -> Result<bool, SetupError> {
    let pages = setup_message_pages(message);
    let mut page = 0usize;
    let mut notice = None;
    loop {
        let text = format!("Page {}/{}\n{}", page + 1, pages.len(), pages[page]);
        terminal.draw_setup(0, title, &text, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if page + 1 < pages.len() => page += 1,
            TerminalInput::Down | TerminalInput::PageDown if page + 1 < pages.len() => page += 1,
            TerminalInput::Up | TerminalInput::PageUp => page = page.saturating_sub(1),
            TerminalInput::Submit => return Ok(true),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Resize => {
                page = 0;
                notice = Some("Terminal resized; reread all pages");
            }
            _ => {}
        }
    }
}

fn setup_message_pages(message: &str) -> Vec<String> {
    let pages: Vec<_> = message
        .lines()
        .flat_map(|line| {
            let chars: Vec<_> = line.chars().collect();
            if chars.is_empty() {
                return vec![String::new()];
            }
            chars
                .chunks(SETUP_MESSAGE_PAGE_CHARS)
                .map(|chunk| chunk.iter().collect())
                .collect()
        })
        .collect();
    if pages.is_empty() {
        vec![String::new()]
    } else {
        pages
    }
}

pub(in super::super) async fn await_operation<T>(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    operation: impl Future<Output = T>,
) -> Result<Option<T>, SetupError> {
    tokio::pin!(operation);
    loop {
        terminal.draw_setup(4, title, instruction, 0, None)?;
        tokio::select! {
            result = &mut operation => return Ok(Some(result)),
            input = terminal.next_input() => match input? {
                TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                    terminal.discard_draft_input();
                    return Ok(None);
                }
                TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
                TerminalInput::Suspend => terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?,
                TerminalInput::LineRejected | TerminalInput::LineContinued => terminal.discard_draft_input(),
                _ => {}
            },
        }
    }
}

pub(super) async fn choose(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
) -> Result<Option<u8>, SetupError> {
    choose_focused(terminal, step, title, instruction, choices, None).await
}

pub(in super::super) async fn choose_focused(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    current: Option<u8>,
) -> Result<Option<u8>, SetupError> {
    if terminal.is_linear() {
        let linear_choices = linear_rendered_choices(choices);
        let rendered = linear_choices
            .iter()
            .map(|(value, label)| (*value, label.as_str()))
            .collect::<Vec<_>>();
        let listing = linear_choices
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if crate::terminal::setup_choices_need_paging(instruction, &rendered)
            && !await_setup_ack(terminal, title, &format!("{instruction}\n{listing}")).await?
        {
            return Ok(None);
        }
    }
    choose_display(terminal, step, title, instruction, choices, current).await
}

async fn choose_display(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    current: Option<u8>,
) -> Result<Option<u8>, SetupError> {
    let selected = choices
        .iter()
        .position(|(value, _)| Some(*value) == current)
        .unwrap_or(0);
    if terminal.is_linear() {
        choose_linear(terminal, step, title, instruction, choices, selected).await
    } else {
        choose_inline(terminal, step, title, instruction, choices, selected).await
    }
}

async fn choose_linear(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    mut selected: usize,
) -> Result<Option<u8>, SetupError> {
    let linear_choices = linear_rendered_choices(choices);
    let linear_rendered = linear_choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let mut input = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(
            step,
            title,
            instruction,
            &linear_rendered,
            selected,
            notice.take(),
        )?;
        match terminal.next_input().await? {
            TerminalInput::Character(character) => {
                if !append_linear_choice_character(&mut input, &mut rejected, character) {
                    notice = Some("Invalid choice input; use Backspace to correct");
                }
            }
            TerminalInput::Backspace => {
                if rejected > 0 {
                    rejected -= 1;
                } else {
                    input.pop();
                }
            }
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down | TerminalInput::Tab => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = choices.len() - 1,
            TerminalInput::Submit => {
                if rejected == 0 && input.trim().is_empty() {
                    return Ok(Some(choices[selected].0));
                }
                if rejected == 0
                    && let Some(value) = linear_choice_value(input.trim(), choices)
                {
                    return Ok(Some(value));
                }
                terminal.discard_draft_input();
                input.clear();
                rejected = 0;
                notice = Some("Type a listed choice name; Ctrl+C cancels");
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                input.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Invalid choice line; Ctrl+C cancels");
            }
            TerminalInput::Suspend => terminal
                .suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES.saturating_sub(input.len()))?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
}

fn append_linear_choice_character(input: &mut String, rejected: &mut u16, character: char) -> bool {
    if *rejected == 0
        && (character.is_ascii_graphic() || character == ' ')
        && input.len() < MAX_SETUP_CHOICE_INPUT_BYTES
    {
        input.push(character);
        true
    } else {
        *rejected = rejected.saturating_add(1);
        false
    }
}

fn linear_rendered_choices(choices: &[(u8, &str)]) -> Vec<(u8, String)> {
    choices
        .iter()
        .enumerate()
        .map(|(index, (value, label))| (*value, format!("{}. {label}", index.saturating_add(1))))
        .collect()
}

fn linear_choice_value(input: &str, choices: &[(u8, &str)]) -> Option<u8> {
    if let Ok(index) = input.parse::<usize>()
        && let Some((value, _)) = index.checked_sub(1).and_then(|index| choices.get(index))
    {
        return Some(*value);
    }
    choices
        .iter()
        .find(|(_, label)| label.eq_ignore_ascii_case(input))
        .map(|(value, _)| *value)
}

async fn choose_inline(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    mut selected: usize,
) -> Result<Option<u8>, SetupError> {
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(step, title, instruction, choices, selected, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Character(_) => notice = Some("Use Up/Down and Enter to select"),
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down | TerminalInput::Tab => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = choices.len() - 1,
            TerminalInput::Submit => return Ok(Some(choices[selected].0)),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
            }
            _ => {}
        }
    }
}

pub(in super::super) async fn read_setup_value(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    initial: &str,
    limit: usize,
) -> Result<Option<String>, SetupError> {
    read_setup_value_inner(
        terminal,
        SetupValueInput {
            title,
            instruction,
            initial,
            limit,
            allow_empty: false,
        },
    )
    .await
}

pub(in super::super) async fn read_optional_setup_value(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    initial: &str,
    limit: usize,
) -> Result<Option<String>, SetupError> {
    read_setup_value_inner(
        terminal,
        SetupValueInput {
            title,
            instruction,
            initial,
            limit,
            allow_empty: true,
        },
    )
    .await
}

struct SetupValueInput<'a> {
    title: &'a str,
    instruction: &'a str,
    initial: &'a str,
    limit: usize,
    allow_empty: bool,
}

async fn read_setup_value_inner(
    terminal: &mut AttachedTerminal,
    input: SetupValueInput<'_>,
) -> Result<Option<String>, SetupError> {
    let SetupValueInput {
        title,
        instruction,
        initial,
        limit,
        allow_empty,
    } = input;
    if !initial.is_empty()
        && !initial.starts_with("re-enter ")
        && !await_setup_ack(
            terminal,
            title,
            &format!(
                "Current value: {initial}\nEnter a replacement; empty Enter {}.",
                if allow_empty { "clears it" } else { "keeps it" }
            ),
        )
        .await?
    {
        return Ok(None);
    }
    let mut composer = Composer::default();
    let prompt = if allow_empty {
        format!("{instruction}; empty Enter clears this field")
    } else if !initial.is_empty() && !initial.starts_with("re-enter ") {
        format!("{instruction}; Enter replaces it, empty Enter keeps it")
    } else {
        instruction.to_owned()
    };
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_text_input(title, &prompt, &composer, notice.take())?;
        let input = terminal.next_input().await?;
        match input {
            TerminalInput::Character(character) if !character.is_control() => {
                composer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Paste => match terminal.take_paste() {
                Ok(text)
                    if !text.chars().any(char::is_control)
                        && composer.text().len().saturating_add(text.len()) <= limit =>
                {
                    composer
                        .insert_paste(&text)
                        .map_err(|_| "setup field is too long")?;
                }
                _ => notice = Some("Field paste is invalid or over the size limit"),
            },
            TerminalInput::Submit if !composer.text().trim().is_empty() => {
                let value = composer.text().trim().to_owned();
                if value.len() <= limit && !value.chars().any(char::is_control) {
                    return Ok(Some(value));
                }
                notice = Some("Field is invalid or over the size limit");
            }
            TerminalInput::Submit if allow_empty => return Ok(Some(String::new())),
            TerminalInput::Submit if !initial.is_empty() => return Ok(Some(initial.to_owned())),
            TerminalInput::Submit => notice = Some("This field cannot be empty"),
            TerminalInput::Backspace
            | TerminalInput::Left
            | TerminalInput::Right
            | TerminalInput::Delete => {
                composer.apply(input);
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Suspend => terminal.suspend_and_resume(
                crate::terminal::MAX_DRAFT_BYTES.saturating_sub(composer.text().len()),
            )?,
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_pages_show_complete_content_at_bounded_width() {
        let message = format!("Destination: {}\nDisposition: Uncertain", "x".repeat(160));
        let pages = setup_message_pages(&message);
        assert!(
            pages
                .iter()
                .all(|page| page.chars().count() <= SETUP_MESSAGE_PAGE_CHARS)
        );
        assert_eq!(pages.concat(), message.replace('\n', ""));
    }
}
