use super::{
    SetupError, append_linear_choice_character, linear_choice_value, linear_rendered_choices,
    setup_message_pages,
};
use crate::terminal::{AttachedTerminal, TerminalInput};

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
