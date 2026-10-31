use super::{AttachedTerminal, Composer, TerminalInput, view};
use crate::presentation::escape_terminal;
#[derive(Debug, thiserror::Error)]
pub enum RewindPanelError {
    #[error(transparent)]
    Terminal(#[from] super::TerminalError),
    #[error("terminated by {0}")]
    Shutdown(&'static str),
}
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Clear, List, ListItem, ListState, Paragraph},
};

#[derive(Clone, Copy)]
pub(super) struct RewindFrame<'a> {
    pub title: &'a str,
    pub lines: &'a [String],
    pub choices: &'a [String],
    pub selected: usize,
    pub page: usize,
    pub palette: view::Palette,
}

pub(super) fn draw_rewind_frame(frame: &mut Frame, area: Rect, state: RewindFrame<'_>) -> bool {
    let RewindFrame {
        title,
        lines,
        choices,
        selected,
        palette,
        ..
    } = state;
    frame.render_widget(Clear, area);
    if !view::dialog_fits(area) {
        frame.render_widget(Paragraph::new("Resize; Esc closes"), area);
        return true;
    }
    if !lines.is_empty() {
        return draw_rewind_review(frame, area, &state);
    }
    frame.render_widget(
        Paragraph::new(title).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rows: Vec<_> = choices
        .iter()
        .enumerate()
        .map(|(index, label)| ListItem::new(Line::raw(format!("{}. {label}", index + 1))))
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_style(palette.selection())
            .highlight_symbol("> "),
        Rect::new(area.x, area.y + 2, area.width, area.height - 3),
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(if area.width >= 80 {
            "Rewind · ↑↓ choose · Enter actions · Esc cancel"
        } else if area.width >= 50 {
            "↑↓ choose · Enter actions · Esc cancel"
        } else {
            "↑↓ Enter Esc"
        }),
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
    );
    false
}

// With action lines, the rewind dialog reuses the paged permission frame and short choice labels.
fn draw_rewind_review(frame: &mut Frame, area: Rect, state: &RewindFrame<'_>) -> bool {
    let RewindFrame {
        lines,
        choices,
        selected,
        page,
        palette,
        ..
    } = *state;
    let labels: Vec<_> = choices
        .iter()
        .map(|choice| {
            if area.width < 50 {
                match choice.as_str() {
                    "Restore code and conversation" => "Code + chat",
                    "Restore conversation" => "Conversation",
                    "Restore code" => "Code",
                    crate::presentation::rewind::KEEP => "Keep files",
                    crate::presentation::rewind::OVERWRITE | "Overwrite after saving" => {
                        "Overwrite"
                    }
                    "Resolve listed files" => "Resolve files",
                    "Reset checkpoint storage" => "Reset storage",
                    other => other,
                }
            } else {
                choice.as_str()
            }
        })
        .collect();
    view::draw_permission_frame(
        frame,
        area,
        lines,
        view::PermissionFrameState {
            choices: &labels,
            page,
            focus: selected,
            profile_start: None,
            profile_count: 0,
            focused_profile: None,
            profile_selection_enabled: None,
            footer: Some(if area.width >= 80 {
                "Rewind · ↑↓ choose · Enter/PgDn review · PgUp back · Esc cancel"
            } else if area.width >= 50 {
                "↑↓ choose · Enter/PgDn review · Esc cancel"
            } else if area.width >= 24 {
                "↑↓ Enter/PgDn Esc"
            } else {
                "↑↓ Enter Esc"
            }),
            palette,
            boxed: false,
        },
    )
    .more
}

impl AttachedTerminal {
    pub fn show_rewind_draft(&mut self, composer: &Composer) -> Result<(), super::TerminalError> {
        if let Some(linear) = &mut self.linear {
            linear.write_clipboard_draft(&mut std::io::stderr(), composer)?;
        }
        Ok(())
    }

    fn draw_rewind(&mut self, state: RewindFrame<'_>) -> Result<bool, RewindPanelError> {
        let mut more = true;
        self.draw_frame(12, |frame, area| {
            more = draw_rewind_frame(frame, area, state);
        })?;
        Ok(more)
    }

    fn draw_linear_rewind(
        &mut self,
        id: uuid::Uuid,
        title: &str,
        lines: &[String],
        choices: &[String],
    ) -> Result<(), RewindPanelError> {
        let linear = self.linear.as_mut().expect("linear terminal");
        let mut rows = vec![format!("Rewind: {}", escape_terminal(title))];
        rows.extend(lines.iter().cloned());
        rows.extend(
            choices
                .iter()
                .enumerate()
                .map(|(i, label)| format!("{}. {label}", i + 1)),
        );
        linear
            .draw_permission_prompt(
                id,
                &rows,
                "Type a number or visible choice name; empty Enter cancels; q cancels:",
            )
            .map_err(super::TerminalError::Io)?;
        Ok(())
    }

    pub async fn rewind_choice(
        &mut self,
        title: &str,
        lines: &[String],
        choices: &[String],
        initial: usize,
    ) -> Result<Option<usize>, RewindPanelError> {
        self.cancel_clipboard_paste();
        let lines = escaped_rewind_lines(title, lines);
        let choices: Vec<_> = choices.iter().map(|line| escape_terminal(line)).collect();
        let mut selected = initial.min(choices.len().saturating_sub(1));
        let mut page = 0usize;
        let mut answer = Composer::default();
        let mut id = uuid::Uuid::now_v7();
        loop {
            let more = if self.linear.is_some() {
                self.draw_linear_rewind(id, title, &lines, &choices)?;
                false
            } else {
                let palette = self.palette();
                self.draw_rewind(RewindFrame {
                    title,
                    lines: &lines,
                    choices: &choices,
                    selected,
                    page,
                    palette,
                })?
            };
            let linear = self.is_linear();
            match self.next_input().await? {
                TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                    return Ok(None);
                }
                TerminalInput::Shutdown(signal) => {
                    return Err(RewindPanelError::Shutdown(signal.name()));
                }
                TerminalInput::Suspend => {
                    self.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                    page = 0;
                    selected = initial;
                    id = uuid::Uuid::now_v7();
                }
                TerminalInput::Resize => {
                    page = 0;
                    selected = initial;
                }
                TerminalInput::Up if !linear && !more => selected = selected.saturating_sub(1),
                TerminalInput::Down if !linear && !more => {
                    selected = (selected + 1).min(choices.len().saturating_sub(1))
                }
                TerminalInput::PageUp if !linear => page = page.saturating_sub(1),
                TerminalInput::PageDown | TerminalInput::Submit if !linear && more => page += 1,
                TerminalInput::Submit if !linear => return Ok(Some(selected)),
                TerminalInput::Character(c) if linear => {
                    answer.apply(TerminalInput::Character(c));
                }
                TerminalInput::Backspace if linear => {
                    answer.apply(TerminalInput::Backspace);
                }
                TerminalInput::LineRejected => {
                    answer.clear();
                    id = uuid::Uuid::now_v7();
                }
                TerminalInput::Submit if linear => {
                    match linear_rewind_answer(&answer.take(), &choices) {
                        LinearAnswer::Cancel => return Ok(None),
                        LinearAnswer::Choice(choice) => return Ok(Some(choice)),
                        LinearAnswer::Invalid => {}
                    }
                    id = uuid::Uuid::now_v7();
                }
                _ => {}
            }
        }
    }
}

enum LinearAnswer {
    Cancel,
    Choice(usize),
    Invalid,
}

// Empty, q or cancel cancels; a number or a choice name picks it.
fn linear_rewind_answer(value: &str, choices: &[String]) -> LinearAnswer {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("cancel") {
        return LinearAnswer::Cancel;
    }
    value
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|i| *i < choices.len())
        .or_else(|| {
            choices
                .iter()
                .position(|choice| choice.eq_ignore_ascii_case(value))
        })
        .map_or(LinearAnswer::Invalid, LinearAnswer::Choice)
}

// Action lines lead with the escaped title; without them the list draws its own title.
fn escaped_rewind_lines(title: &str, lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        Vec::new()
    } else {
        std::iter::once(escape_terminal(title))
            .chain(lines.iter().map(|line| escape_terminal(line)))
            .collect()
    }
}
