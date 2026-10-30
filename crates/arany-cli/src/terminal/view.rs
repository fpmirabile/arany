mod agents;
mod approval;
mod composer;
mod frame;
mod model_picker;
mod pickers;
mod setup;
mod transcript;

use super::commands::{Submission, help_entry, help_len};
use super::history::{HistoryRow, MAX_ROW_CELLS, MessageKind, native_marker};
#[cfg(test)]
use super::input::KeyboardSupport;
use super::{Composer, TerminalMode, backend::TerminalBackend, input::TerminalQueries};
use crate::presentation::{
    AgentInspectorModel, DraftAction, MAX_ROW_WIDTH, PresentationModel, model_id_preview,
    safe_truncate,
};
pub(super) use agents::*;
pub(super) use approval::*;
#[cfg(test)]
use arany::ModelEntry;
use arany::SessionListItem;
pub(super) use composer::*;
pub(super) use frame::*;
pub(super) use model_picker::*;
pub(super) use pickers::*;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::Backend,
    buffer::CellDiffOption,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};
pub(super) use setup::*;
use std::io;
pub(super) use transcript::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// Wraps at spaces; a token wider than the row hard-breaks at the row edge.
pub(super) fn wrap_cells(text: &str, width: usize, indent: usize) -> Vec<String> {
    let indent = indent.min(width.saturating_sub(1));
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    let mut fresh = true;
    for word in text.split(' ') {
        let word_cells = UnicodeWidthStr::width(word);
        if !fresh && cells + 1 + word_cells > width {
            rows.push(std::mem::replace(&mut row, " ".repeat(indent)));
            cells = indent;
            fresh = true;
        }
        if cells + usize::from(!fresh) + word_cells <= width {
            if !fresh {
                row.push(' ');
                cells += 1;
            }
            row.push_str(word);
            cells += word_cells;
            fresh = false;
            continue;
        }
        for grapheme in word.graphemes(true) {
            let count = UnicodeWidthStr::width(grapheme);
            let base = if rows.is_empty() { 0 } else { indent };
            if cells + count > width && cells > base {
                rows.push(std::mem::replace(&mut row, " ".repeat(indent)));
                cells = indent;
            }
            row.push_str(grapheme);
            cells += count;
        }
        fresh = false;
    }
    rows.push(row);
    rows
}

pub(super) fn focus_marker(focused: bool) -> &'static str {
    if focused { "›" } else { " " }
}

fn numbered_label(index: usize, label: &str, focused: bool) -> String {
    format!("{} {}. {label}", focus_marker(focused), index + 1)
}

fn choice_style(focus: Option<usize>, index: usize) -> Style {
    match focus {
        None => Style::default().add_modifier(Modifier::DIM),
        Some(focused) if focused == index => Style::default().add_modifier(Modifier::REVERSED),
        Some(_) => Style::default(),
    }
}

// An unarmed selector shows dim rows with no focus until its input barrier completes.
fn numbered_line(index: usize, label: &str, focus: Option<usize>, width: u16) -> Line<'static> {
    Line::styled(
        safe_truncate(
            &numbered_label(index, label, focus == Some(index)),
            usize::from(width),
        ),
        choice_style(focus, index),
    )
}

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) color: bool,
}

impl Palette {
    pub(super) fn accent(self) -> Style {
        if self.color {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    pub(super) fn attention(self) -> Style {
        if self.color {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    }

    fn error(self) -> Style {
        if self.color {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        }
    }

    pub(super) fn selection(self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    fn subtle(self) -> Style {
        if self.color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        }
    }
}

fn accent_lead(line: String, palette: Palette) -> Line<'static> {
    if let Some((lead, rest)) = line.split_once(" · ") {
        Line::from(vec![
            Span::styled(lead.to_owned(), palette.accent()),
            Span::raw(format!(" · {rest}")),
        ])
    } else {
        Line::styled(line, palette.accent())
    }
}

fn slash_spans(text: String, cells: usize, style: Style, palette: Palette) -> Vec<Span<'static>> {
    // Recognized slash tokens are ASCII, so their byte lengths equal character counts.
    let split = text
        .char_indices()
        .nth(cells)
        .map_or(text.len(), |(byte, _)| byte);
    vec![
        Span::styled(text[..split].to_owned(), style.patch(palette.selection())),
        Span::styled(text[split..].to_owned(), style),
    ]
}

pub(super) fn screen_terminal(
    mode: TerminalMode,
    queries: Option<TerminalQueries>,
) -> io::Result<Terminal<TerminalBackend>> {
    let mut backend = TerminalBackend::new(mode, queries);
    let screen = backend.size()?;
    if mode == TerminalMode::Classic {
        // Let inline initialization scroll to reserve rows without moving above shell output.
        backend.set_cursor_position((0, screen.height.saturating_sub(1)))?;
    }
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: match mode {
                TerminalMode::Classic => Viewport::Inline(screen.height.min(4)),
                TerminalMode::Fullscreen => Viewport::Fullscreen,
            },
        },
    )?;
    let area = terminal.get_frame().area();
    terminal.backend_mut().set_area(area);
    terminal.backend_mut().clear_owned()?;
    Ok(terminal)
}

pub(super) fn bottom_area(area: Rect, height: u16) -> Rect {
    let height = area.height.min(height);
    Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    )
}

// Classic selector lists show at most this many rows and scroll inside, like Codex's
// MAX_POPUP_ROWS; ten matches the page size of the linear pickers.
pub const SELECTOR_ROWS: usize = 10;

// From this width dialogs keep side gutters and full labels and hints; narrower ones shorten.
pub(super) const FULL_LAYOUT_WIDTH: u16 = 40;

// Every selector and confirmation fits this area; a smaller one asks for a resize.
pub(super) const MIN_DIALOG_WIDTH: u16 = 16;
pub(super) const MIN_DIALOG_HEIGHT: u16 = 8;

pub(super) fn dialog_fits(area: Rect) -> bool {
    area.width >= MIN_DIALOG_WIDTH && area.height >= MIN_DIALOG_HEIGHT
}

// The first shown row of a window that keeps the focused row in view.
pub(super) fn window_start(selected: Option<usize>, visible: usize) -> usize {
    selected.map_or(0, |index| index.saturating_sub(visible.saturating_sub(1)))
}

#[cfg(test)]
mod tests;
