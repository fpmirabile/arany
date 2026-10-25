use super::{AttachedTerminal, TerminalError, TerminalInput, view};
use arany::SessionView;
use ratatui::{layout::Rect, widgets::Paragraph};
use std::io::{self, Write};

impl AttachedTerminal {
    pub async fn show_local_report(
        &mut self,
        session: &SessionView,
        text: &str,
    ) -> Result<(), TerminalError> {
        self.cancel_clipboard_paste();
        if let Some(linear) = &mut self.linear {
            io::stderr().write_all(text.as_bytes())?;
            linear.prompt_needed = true;
            return Ok(());
        }
        self.begin_dialog();
        let mut offset = 0usize;
        loop {
            let mut page = 1;
            let mut last = 0;
            let width = self.owned.terminal().size()?.width;
            let rows = text
                .lines()
                .flat_map(|line| view::wrap_cells(line, usize::from(width.max(1)), 0))
                .collect::<Vec<_>>();
            let height = u16::try_from(rows.len().saturating_add(2)).unwrap_or(u16::MAX);
            let armed = self.dialog_armed;
            self.draw_dialog(Some(session), height, |frame, area| {
                page = usize::from(area.height.saturating_sub(2).max(1));
                last = rows.len().saturating_sub(page);
                offset = offset.min(last);
                frame.render_widget(
                    Paragraph::new(rows[offset..rows.len().min(offset + page)].join("\n")),
                    Rect {
                        height: area.height.saturating_sub(2),
                        ..area
                    },
                );
                let footer = if armed {
                    format!("Esc | {}/{} | Up/Dn PgUp/PgDn", offset + 1, rows.len())
                } else {
                    "Opening storage report".to_owned()
                };
                frame.render_widget(
                    Paragraph::new(footer),
                    Rect {
                        y: area.bottom().saturating_sub(1),
                        height: 1,
                        ..area
                    },
                );
            })?;
            if self.arm_dialog().await? {
                continue;
            }
            match self.next_input().await? {
                TerminalInput::Up => offset = offset.saturating_sub(1),
                TerminalInput::Down => offset = (offset + 1).min(last),
                TerminalInput::PageUp => offset = offset.saturating_sub(page),
                TerminalInput::PageDown => offset = (offset + page).min(last),
                TerminalInput::Home => offset = 0,
                TerminalInput::End => offset = last,
                TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::Submit => {
                    return Ok(());
                }
                input @ (TerminalInput::Shutdown(_) | TerminalInput::EndOfInput) => {
                    self.defer_input(input);
                    return Ok(());
                }
                TerminalInput::Suspend => self.suspend_and_resume(0)?,
                _ => {}
            }
        }
    }
}
