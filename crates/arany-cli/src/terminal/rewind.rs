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

#[expect(
    clippy::too_many_arguments,
    reason = "dialog content and its explicit rendering bounds stay together"
)]
pub(super) fn draw_rewind_frame(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    choices: &[String],
    selected: usize,
    page: usize,
    palette: view::Palette,
) -> bool {
    frame.render_widget(Clear, area);
    if area.width < 16 || area.height < 8 {
        frame.render_widget(Paragraph::new("Resize; Esc closes"), area);
        return true;
    }
    if !lines.is_empty() {
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
        return view::draw_permission_frame(
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
        .more;
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

impl AttachedTerminal {
    pub fn show_rewind_draft(&mut self, composer: &Composer) -> Result<(), super::TerminalError> {
        if let Some(linear) = &mut self.linear {
            linear.write_clipboard_draft(&mut std::io::stderr(), composer)?;
        }
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
        let lines: Vec<_> = if lines.is_empty() {
            Vec::new()
        } else {
            std::iter::once(escape_terminal(title))
                .chain(lines.iter().map(|line| escape_terminal(line)))
                .collect()
        };
        let choices: Vec<_> = choices.iter().map(|line| escape_terminal(line)).collect();
        let mut selected = initial.min(choices.len().saturating_sub(1));
        let mut page = 0usize;
        let mut answer = Composer::default();
        let mut id = uuid::Uuid::now_v7();
        loop {
            let more = if let Some(linear) = &mut self.linear {
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
                false
            } else {
                let palette = self.palette();
                let mut more = true;
                self.draw_frame(12, true, |frame, area| {
                    more = draw_rewind_frame(
                        frame, area, title, &lines, &choices, selected, page, palette,
                    );
                })?;
                more
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
                    self.suspend_and_resume(0)?;
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
                    let value = answer.take();
                    let value = value.trim();
                    if value.is_empty()
                        || value.eq_ignore_ascii_case("q")
                        || value.eq_ignore_ascii_case("cancel")
                    {
                        return Ok(None);
                    }
                    let choice = value
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .filter(|i| *i < choices.len())
                        .or_else(|| {
                            choices
                                .iter()
                                .position(|choice| choice.eq_ignore_ascii_case(value))
                        });
                    if let Some(choice) = choice {
                        return Ok(Some(choice));
                    }
                    id = uuid::Uuid::now_v7();
                }
                _ => {}
            }
        }
    }
}
