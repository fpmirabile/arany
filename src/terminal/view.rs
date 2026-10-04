use super::Composer;
use super::commands::{Submission, help_entry, help_len, parse_submission};
use super::history::{HistoryRow, MessageKind};
use crate::presentation::{
    AgentInspectorModel, DraftAction, PresentationModel, model_id_preview, safe_truncate,
};
#[cfg(test)]
use crate::provider::ModelEntry;
use crate::session::SessionListItem;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::io::{self, Stderr};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_VISIBLE_DRAFT_ROWS: usize = 4;

struct ComposerRows {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_cells: usize,
}

fn wrapped_composer(composer: &Composer, width: usize) -> ComposerRows {
    let mut display = ComposerRows {
        lines: vec![String::new()],
        cursor_row: 0,
        cursor_cells: 0,
    };
    if width == 0 {
        return display;
    }
    let mut cells = 0;
    for (offset, grapheme) in composer.text().grapheme_indices(true) {
        if grapheme == "\n" {
            if offset == composer.cursor_byte_offset() {
                display.cursor_row = if cells < width {
                    display.lines.len() - 1
                } else {
                    display.lines.len()
                };
                display.cursor_cells = if cells < width { cells } else { 0 };
            }
            display.lines.push(String::new());
            cells = 0;
            continue;
        }
        let safe = safe_truncate(if grapheme == "\t" { "    " } else { grapheme }, width);
        let grapheme_cells = UnicodeWidthStr::width(safe.as_str());
        if cells + grapheme_cells > width {
            display.lines.push(String::new());
            cells = 0;
        }
        if offset == composer.cursor_byte_offset() {
            display.cursor_row = display.lines.len() - 1;
            display.cursor_cells = cells;
        }
        display.lines.last_mut().expect("draft row").push_str(&safe);
        cells += grapheme_cells;
    }
    if cells == width {
        display.lines.push(String::new());
        cells = 0;
    }
    if composer.cursor_byte_offset() == composer.text().len() {
        display.cursor_row = display.lines.len() - 1;
        display.cursor_cells = cells;
    }
    display
}

pub(super) fn composer_height(composer: &Composer, area: Rect) -> u16 {
    let rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    (rows.lines.len().min(MAX_VISIBLE_DRAFT_ROWS) as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(1))
}

pub(super) fn completion_height(composer: &Composer, available_height: u16) -> u16 {
    composer.completion_menu().map_or(0, |(names, _)| {
        (names.len().min(3) as u16).min(available_height.saturating_sub(1))
    })
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

    fn attention(self) -> Style {
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

    fn selection(self) -> Style {
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

pub(super) fn screen_terminal() -> io::Result<Terminal<CrosstermBackend<Stderr>>> {
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stderr()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    terminal.clear()?;
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

fn setup_area(full: Rect) -> Rect {
    let height = full.height.min(if full.width < 20 { 8 } else { 7 });
    let width = full.width.min(64);
    Rect::new(
        full.x.saturating_add((full.width - width) / 2),
        full.y + full.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(super) fn draw_setup_frame(
    frame: &mut Frame,
    step: u8,
    title: &str,
    instruction: &str,
    input_bytes: usize,
    notice: Option<&str>,
    palette: Palette,
) {
    let area = setup_area(frame.area());
    if area.width == 0 || area.height == 0 {
        return;
    }
    let status = if matches!(step, 0 | 4 | 6) {
        "Please wait".to_owned()
    } else if area.width < 16 {
        format!("{input_bytes} B")
    } else if area.width < 20 {
        format!("{input_bytes}B Enter Esc")
    } else if area.width < 50 {
        format!("{input_bytes}B · Enter · Esc")
    } else {
        format!("Input: {input_bytes} bytes · Enter confirms · Esc cancels")
    };
    frame.render_widget(
        Paragraph::new(Line::styled("Arany setup", palette.accent())),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                safe_truncate(title, usize::from(area.width)),
                palette.selection(),
            )),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    if area.height > 4 {
        frame.render_widget(
            Paragraph::new(safe_truncate(instruction, 240)).wrap(Wrap { trim: false }),
            Rect::new(area.x, area.y + 2, area.width, area.height - 4),
        );
        frame.render_widget(
            Paragraph::new(safe_truncate(&status, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 2, area.width, 1),
        );
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(safe_truncate(notice, usize::from(area.width))).style(
                if notice.starts_with("Error:") {
                    palette.error()
                } else {
                    palette.attention()
                },
            ),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn draw_setup_text_frame(
    frame: &mut Frame,
    title: &str,
    instruction: &str,
    draft: &Composer,
    notice: Option<&str>,
    palette: Palette,
) {
    let area = setup_area(frame.area());
    if area.width < 3 || area.height < 6 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled("Arany setup", palette.accent())),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            safe_truncate(title, usize::from(area.width)),
            palette.selection(),
        )),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(safe_truncate(instruction, usize::from(area.width))),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
    let rows = wrapped_composer(draft, usize::from(area.width - 2));
    let visible_rows = usize::from(area.height - 5);
    let start = rows.cursor_row.saturating_sub(visible_rows - 1);
    for (index, line) in rows.lines.iter().skip(start).take(visible_rows).enumerate() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if index == 0 { "> " } else { "  " }, palette.accent()),
                if draft.is_empty() {
                    Span::styled("wrkspc_...", palette.subtle())
                } else {
                    Span::raw(line.clone())
                },
            ])),
            Rect::new(area.x, area.y + 3 + index as u16, area.width, 1),
        );
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(safe_truncate(notice, usize::from(area.width))).style(
                if notice.starts_with("Error:") {
                    palette.error()
                } else {
                    palette.attention()
                },
            ),
            Rect::new(area.x, area.bottom() - 2, area.width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(if area.width < 40 {
            "Enter · Esc"
        } else {
            "Enter confirms · Esc cancels"
        }),
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
    );
    frame.set_cursor_position(Position {
        x: area.x + 2 + rows.cursor_cells as u16,
        y: area.y + 3 + (rows.cursor_row - start) as u16,
    });
}

pub(super) fn draw_setup_choices_frame(
    frame: &mut Frame,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    selected: usize,
    notice: Option<&str>,
    palette: Palette,
) {
    let full = frame.area();
    let height = full.height.min(8);
    let width = full.width.min(64);
    let area = Rect::new(
        full.x.saturating_add((full.width - width) / 2),
        full.y + full.height.saturating_sub(height) / 2,
        width,
        height,
    );
    if area.width == 0 || area.height < 3 || choices.is_empty() {
        return;
    }
    let compact = area.width < 20;
    let instruction = safe_truncate(instruction, 240);
    let supporting_rows = if instruction.width() <= usize::from(area.width) {
        4
    } else {
        5
    };
    let dense = choices.len() > usize::from(area.height.saturating_sub(supporting_rows));
    let header = if dense || compact {
        "↑↓ Enter Esc"
    } else {
        "Arany"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(header, palette.accent())),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            safe_truncate(title, usize::from(area.width)),
            palette.selection(),
        )),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    let row_count = if dense {
        usize::from(area.height - 2)
    } else {
        choices.len()
    };
    let first = selected
        .saturating_sub(row_count.saturating_sub(1))
        .min(choices.len().saturating_sub(row_count));
    for (offset, (_, label)) in choices.iter().enumerate().skip(first).take(row_count) {
        let highlighted = offset == selected;
        let line = format!("{} {label}", if highlighted { '>' } else { ' ' });
        frame.render_widget(
            Paragraph::new(safe_truncate(&line, usize::from(area.width))).style(if highlighted {
                palette.selection()
            } else {
                Style::default()
            }),
            Rect::new(area.x, area.y + 2 + (offset - first) as u16, area.width, 1),
        );
    }
    if !dense {
        let instruction_y = area.y + 2 + row_count as u16;
        let instruction_height = area.bottom().saturating_sub(instruction_y + 1);
        if instruction_height > 0 {
            frame.render_widget(
                Paragraph::new(instruction).wrap(Wrap { trim: false }),
                Rect::new(area.x, instruction_y, area.width, instruction_height),
            );
        }
        let footer = notice.unwrap_or(if compact {
            "↑↓ Enter Esc"
        } else {
            "Up/Down · Enter selects · Esc closes"
        });
        frame.render_widget(
            Paragraph::new(safe_truncate(footer, usize::from(area.width))).style(
                if notice.is_some() {
                    palette.attention()
                } else {
                    Style::default()
                },
            ),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn draw_setup_warning_frame(
    frame: &mut Frame,
    warning: &str,
    page: usize,
    accept_selected: bool,
    notice: Option<&str>,
    palette: Palette,
) -> bool {
    let full = frame.area();
    let width = full.width.min(72);
    let height = full.height.min(16);
    if width == 0 || height < 8 {
        return false;
    }
    let area = Rect::new(
        full.x.saturating_add((full.width - width) / 2),
        full.y.saturating_add((full.height - height) / 2),
        width,
        height,
    );
    let body_height = height - 4;
    let lines = wrap_ascii_setup_warning(warning, usize::from(width));
    let pages = lines.len().div_ceil(usize::from(body_height));
    let page = page.min(pages - 1);
    let has_more = page + 1 < pages;
    frame.render_widget(
        Paragraph::new(Line::styled("ChatGPT plan consent", palette.selection())),
        Rect::new(area.x, area.y, width, 1),
    );
    let start = page * usize::from(body_height);
    let end = (start + usize::from(body_height)).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[start..end].join("\n")),
        Rect::new(area.x, area.y + 1, width, body_height),
    );
    let prompt = if has_more {
        if width <= 24 {
            format!("Page {}/{} Enter", page + 1, pages)
        } else {
            format!("Page {}/{} · PgDn/Enter", page + 1, pages)
        }
    } else {
        String::new()
    };
    let actions = if !has_more {
        let accept = accept_selected;
        Line::from(vec![
            Span::styled(
                if accept { "[Back] " } else { ">[Back] " },
                if accept {
                    Style::default()
                } else {
                    palette.selection()
                },
            ),
            Span::styled(
                if accept { ">[Accept]" } else { "[Accept]" },
                if accept {
                    palette.selection()
                } else {
                    Style::default()
                },
            ),
        ])
    } else {
        Line::raw(safe_truncate(&prompt, usize::from(width)))
    };
    frame.render_widget(
        Paragraph::new(actions),
        Rect::new(area.x, area.bottom() - 3, width, 1),
    );
    frame.render_widget(
        Paragraph::new(if has_more {
            "Read all pages"
        } else {
            "Tab/Enter"
        }),
        Rect::new(area.x, area.bottom() - 2, width, 1),
    );
    frame.render_widget(
        Paragraph::new(safe_truncate(
            notice.unwrap_or("Esc cancels"),
            usize::from(width),
        ))
        .style(if notice.is_some() {
            palette.attention()
        } else {
            Style::default()
        }),
        Rect::new(area.x, area.bottom() - 1, width, 1),
    );
    has_more
}

fn wrap_ascii_setup_warning(warning: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in warning.split_ascii_whitespace() {
        for chunk in word.as_bytes().chunks(width) {
            let chunk = std::str::from_utf8(chunk).expect("compiled ASCII consent warning");
            if !current.is_empty() && current.len() + 1 + chunk.len() > width {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(chunk);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
pub(super) fn draw_frame(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    notice: Option<&str>,
    palette: Palette,
) {
    draw_frame_with_history(frame, model, composer, notice, palette, &[], None);
}

pub(super) fn draw_frame_with_history(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    notice: Option<&str>,
    palette: Palette,
    history_rows: &[HistoryRow],
    history_status: Option<&str>,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let draft_rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    let composer_height = (draft_rows.lines.len().min(MAX_VISIBLE_DRAFT_ROWS) as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(1));
    let composer_y = area.bottom().saturating_sub(1 + composer_height);
    let menu_height = completion_height(composer, composer_y.saturating_sub(area.y));
    let menu_y = composer_y.saturating_sub(menu_height);
    let activity_rows = menu_y
        .saturating_sub(area.y)
        .min(u16::try_from(model.activity_lines.len()).unwrap_or(u16::MAX));
    let history_height = menu_y.saturating_sub(area.y + activity_rows);
    for (row, line) in history_rows
        .iter()
        .take(usize::from(history_height))
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(line.text.as_str()).style(if line.matched {
                palette.selection()
            } else {
                match line.speaker {
                    Some(MessageKind::User) => Style::default().add_modifier(Modifier::BOLD),
                    Some(MessageKind::Assistant) => palette.selection(),
                    Some(MessageKind::Notice) => palette.accent().add_modifier(Modifier::BOLD),
                    Some(MessageKind::Error) => palette.error().add_modifier(Modifier::BOLD),
                    None => Style::default(),
                }
            }),
            Rect::new(area.x, area.y + row as u16, area.width, 1),
        );
    }
    if history_rows.is_empty()
        && history_status.is_none()
        && model.activity_lines.is_empty()
        && menu_height == 0
    {
        let welcome = if model.setup_required && area.width < 20 {
            ["Arany", "Setup required", "Type /setup"]
        } else if model.setup_required && area.width < 50 {
            ["Arany", "Choose an account.", "Type /setup"]
        } else if model.setup_required {
            [
                "Arany",
                "Choose an account to begin.",
                "Type /setup to select a provider.",
            ]
        } else if area.width < 20 {
            ["Arany", "Describe a task", "/help · Ctrl+K"]
        } else if area.width < 50 {
            [
                "Arany",
                "Describe a task to begin.",
                "/help commands · Ctrl+K actions",
            ]
        } else {
            [
                "Arany",
                "Describe a task to begin.",
                "/help commands · Ctrl+K actions · Ctrl+O newline",
            ]
        };
        let top = area.y + history_height.saturating_sub(4);
        for (index, text) in welcome
            .into_iter()
            .enumerate()
            .take(usize::from(history_height))
        {
            frame.render_widget(
                Paragraph::new(safe_truncate(text, usize::from(area.width))).style(if index == 0 {
                    palette.selection()
                } else {
                    Style::default()
                }),
                Rect::new(area.x, top + index as u16, area.width, 1),
            );
        }
    }
    for (row, line) in (0..activity_rows).zip(&model.activity_lines) {
        frame.render_widget(
            Paragraph::new(accent_lead(
                safe_truncate(line, usize::from(area.width)),
                palette,
            )),
            Rect::new(area.x, menu_y - activity_rows + row, area.width, 1),
        );
    }
    if menu_height > 0
        && let Some((names, selected)) = composer.completion_menu()
    {
        let rows = usize::from(menu_height);
        let start = selected
            .saturating_sub(rows / 2)
            .min(names.len().saturating_sub(rows));
        for (row, name) in names.iter().enumerate().skip(start).take(rows) {
            let focused = row == selected;
            let marker = if focused { "> " } else { "  " };
            frame.render_widget(
                Paragraph::new(safe_truncate(
                    &format!("{marker}/{name}"),
                    usize::from(area.width),
                ))
                .style(if focused {
                    palette.selection()
                } else {
                    Style::default()
                }),
                Rect::new(area.x, menu_y + (row - start) as u16, area.width, 1),
            );
        }
    }
    let composer_area = Rect {
        y: composer_y,
        height: composer_height,
        ..area
    };
    draw_composer(
        frame,
        model,
        composer,
        palette,
        composer_area,
        menu_height,
        false,
    );
    let preview = (!composer.text().contains('\n'))
        .then(|| composer.command_preview())
        .flatten();
    let status_y = area.bottom() - 1;
    if status_y < area.bottom() {
        let wide_history_status = history_status
            .filter(|_| area.width >= 80)
            .map(|status| format!("{status} · {}", model.status_line));
        let status_notice = notice;
        let status = status_notice
            .or(wide_history_status.as_deref())
            .or(history_status)
            .or_else(|| {
                (model.activity_lines.is_empty() && model.draft_action != DraftAction::Retain)
                    .then(|| preview.as_ref().map(|preview| preview.status.as_str()))
                    .flatten()
            })
            .unwrap_or(&model.status_line);
        let safe_status = safe_truncate(status, usize::from(area.width));
        let status = match status_notice {
            Some(message) if message.starts_with("Error:") => {
                Line::styled(safe_status, palette.error())
            }
            Some(_) => Line::styled(safe_status, palette.attention()),
            None => accent_lead(safe_status, palette),
        };
        frame.render_widget(
            Paragraph::new(status),
            Rect::new(area.x, status_y, area.width, 1),
        );
    }
}

pub(super) fn draw_session_picker_frame(
    frame: &mut Frame,
    items: &[SessionListItem],
    selected: usize,
    palette: Palette,
) {
    let area = bottom_area(frame.area(), 12);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let header = format!("Sessions · {} in this Workspace", items.len());
    frame.render_widget(
        Paragraph::new(safe_truncate(&header, usize::from(area.width))).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rows = usize::from(area.height.saturating_sub(2));
    let start = session_picker_start(selected, rows);
    for (row, item) in items.iter().skip(start).take(rows).enumerate() {
        let marker = if start + row == selected { "> " } else { "  " };
        let id = item.id.to_string();
        let suffix_len = if area.width < 20 { 8 } else { 12 };
        let title = safe_truncate(
            &item.title,
            usize::from(area.width).saturating_sub(5 + suffix_len),
        );
        let line = format!("{marker}{title} · {}", &id[id.len() - suffix_len..]);
        frame.render_widget(
            Paragraph::new(safe_truncate(&line, usize::from(area.width))).style(
                if start + row == selected {
                    palette.selection()
                } else {
                    Style::default()
                },
            ),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
    if area.height >= 2 {
        let hint = if area.width < 20 {
            "Up/Dn Enter Esc"
        } else if area.width < 24 {
            "Up/Dn Enter:go Esc"
        } else if area.width < 40 {
            "Up/Dn Enter:resume Esc"
        } else {
            "Up/Down select · Enter resume · Esc close"
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

fn draw_composer(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    palette: Palette,
    composer_area: Rect,
    menu_height: u16,
    picker_focused: bool,
) {
    let area = composer_area;
    let draft_rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    let draft = composer.text();
    let visible_lines = usize::from(composer_area.height.saturating_sub(2));
    let first_line = draft_rows
        .cursor_row
        .saturating_sub(visible_lines / 2)
        .min(draft_rows.lines.len().saturating_sub(visible_lines));
    let preview = (!picker_focused && !draft.contains('\n'))
        .then(|| composer.command_preview())
        .flatten();
    let inner_width = usize::from(area.width.saturating_sub(2));
    let mut rendered_lines = Vec::with_capacity(visible_lines);
    for (index, line) in draft_rows
        .lines
        .iter()
        .enumerate()
        .skip(first_line)
        .take(visible_lines)
    {
        if index == draft_rows.cursor_row {
            let available = inner_width.saturating_sub(UnicodeWidthStr::width(line.as_str()));
            let ghost = preview.as_ref().map_or(String::new(), |preview| {
                if palette.color || preview.ghost.is_empty() || preview.ghost_is_placeholder {
                    safe_truncate(&preview.ghost, available)
                } else if available >= 3 {
                    format!("[{}]", safe_truncate(&preview.ghost, available - 2))
                } else {
                    String::new()
                }
            });
            rendered_lines.push(Line::from(vec![
                Span::raw(line.clone()),
                Span::styled(
                    ghost,
                    if palette.color {
                        Style::default().add_modifier(Modifier::DIM)
                    } else {
                        Style::default()
                    },
                ),
            ]));
        } else {
            rendered_lines.push(Line::raw(line.clone()));
        }
    }
    let mut position = if visible_lines > 0 && draft_rows.lines.len() > visible_lines {
        if composer_area.width >= 40 {
            format!(
                " · rows {}-{}/{}",
                first_line + 1,
                (first_line + visible_lines).min(draft_rows.lines.len()),
                draft_rows.lines.len(),
            )
        } else {
            format!(" {}/{}", draft_rows.cursor_row + 1, draft_rows.lines.len())
        }
    } else {
        String::new()
    };
    if !composer.images().is_empty() {
        position = if composer_area.width >= 40 {
            format!(
                " · {} image{}{}",
                composer.images().len(),
                if composer.images().len() == 1 {
                    ""
                } else {
                    "s"
                },
                position
            )
        } else {
            format!(" {}img{}", composer.images().len(), position)
        };
    }
    let title = if 2 + "Ask Arany".len() + position.len() <= usize::from(composer_area.width) {
        "Ask Arany"
    } else {
        "Draft"
    };
    let lead = if title == "Ask Arany" { "─ " } else { "" };
    let heading_cells = UnicodeWidthStr::width(lead) + title.len() + position.len() + 1;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(lead, palette.subtle()),
            Span::styled(title, palette.selection()),
            Span::raw(position),
            Span::styled(
                format!(
                    " {}",
                    "─".repeat(usize::from(composer_area.width).saturating_sub(heading_cells))
                ),
                palette.subtle(),
            ),
        ])),
        Rect::new(composer_area.x, composer_area.y, composer_area.width, 1),
    );
    if visible_lines > 0 && composer_area.width >= 3 {
        if draft.is_empty() {
            let hint = if !composer.images().is_empty() {
                if composer_area.width < 40 {
                    "Message or Enter"
                } else {
                    "Add a message, or Enter to send image"
                }
            } else if model.setup_required {
                if composer_area.width < 40 {
                    "Type /setup"
                } else {
                    "Type /setup to connect an account"
                }
            } else if composer_area.width < 40 {
                "Message Arany"
            } else {
                "Ask a question, or type / for commands"
            };
            rendered_lines[0] = Line::styled(safe_truncate(hint, inner_width), palette.subtle());
        }
        frame.render_widget(
            Paragraph::new(Line::styled("> ", palette.accent())),
            Rect::new(composer_area.x, composer_area.y + 1, 2, 1),
        );
        let command = model.draft_action != DraftAction::Retain
            && matches!(parse_submission(draft), Ok(Submission::Command { .. }));
        let hint = if picker_focused {
            if composer_area.width < 40 {
                " Draft kept "
            } else {
                " Draft kept · Esc returns "
            }
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 40 {
            " Enter keeps draft · Ctrl+O newline "
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 24 {
            " Enter keeps · Ctrl+O "
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 16 {
            " Enter keeps "
        } else if menu_height > 0 && composer_area.width >= 40 {
            " Up/Down · Tab/Enter fills · Esc close "
        } else if menu_height > 0 && composer_area.width >= 20 {
            " Tab/Enter fills Esc "
        } else if menu_height > 0 {
            " Tab/Enter Esc "
        } else if command && composer_area.width >= 40 {
            " Enter command · Ctrl+O newline "
        } else if command && composer_area.width >= 16 {
            " Enter command "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 40 {
            " Enter keeps draft · Ctrl+O newline "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 24 {
            " Enter keeps · Ctrl+O "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 16 {
            " Enter keeps "
        } else if composer_area.width >= 40 {
            " Enter sends · Ctrl+O newline "
        } else if composer_area.width >= 16 {
            " Enter · Ctrl+O "
        } else {
            ""
        };
        let newline_hint = if composer_area.width >= 80 && menu_height == 0 {
            "· Shift+Enter newline "
        } else {
            ""
        };
        let rules = usize::from(composer_area.width)
            .saturating_sub(UnicodeWidthStr::width(hint) + UnicodeWidthStr::width(newline_hint));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("─".repeat(rules / 2), palette.subtle()),
                Span::raw(hint),
                Span::raw(newline_hint),
                Span::styled("─".repeat(rules - rules / 2), palette.subtle()),
            ])),
            Rect::new(
                composer_area.x,
                composer_area.bottom() - 1,
                composer_area.width,
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(rendered_lines),
            Rect::new(
                composer_area.x + 2,
                composer_area.y + 1,
                composer_area.width - 2,
                u16::try_from(visible_lines).unwrap_or(u16::MAX),
            ),
        );
    }
    if visible_lines > 0 && composer_area.width >= 3 {
        let cursor_x = composer_area
            .x
            .saturating_add(2)
            .saturating_add(u16::try_from(draft_rows.cursor_cells).unwrap_or(u16::MAX))
            .min(composer_area.right().saturating_sub(1));
        frame.set_cursor_position(Position {
            x: cursor_x,
            y: composer_area.y.saturating_add(1).saturating_add(
                u16::try_from(draft_rows.cursor_row - first_line).unwrap_or(u16::MAX),
            ),
        });
    }
}

pub(super) fn draw_quick_actions_frame(
    frame: &mut Frame,
    selected: usize,
    has_draft: bool,
    palette: Palette,
) {
    let area = bottom_area(frame.area(), 6);
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new("Quick actions").style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let narrow = area.width < 40;
    let sessions = if has_draft && area.width < 20 {
        "Sessions draft"
    } else if has_draft && area.width < 24 {
        "Sessions · draft"
    } else if has_draft && narrow {
        "Sessions · draft first"
    } else if has_draft {
        "Sessions · finish draft first"
    } else if area.width < 20 {
        "Sessions: list"
    } else if narrow {
        "Sessions · resume"
    } else {
        "Sessions · resume a Session"
    };
    let models = if area.width < 20 {
        "Models: choose"
    } else if area.width < 24 {
        "Models · choose"
    } else {
        "Models · choose a model"
    };
    let agents = if area.width < 20 {
        "Agents: view"
    } else if area.width < 24 {
        "Agents · inspect"
    } else {
        "Agents · inspect work"
    };
    let setup = if area.width < 24 {
        "Setup: account"
    } else {
        "Setup · connect an account"
    };
    let labels = [models, agents, sessions, setup];
    let visible = usize::from(area.height.saturating_sub(2));
    let first = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(labels.len().saturating_sub(visible));
    for (index, label) in labels.into_iter().enumerate().skip(first).take(visible) {
        let marker = if index == selected { "> " } else { "  " };
        frame.render_widget(
            Paragraph::new(safe_truncate(
                &format!("{marker}{label}"),
                usize::from(area.width),
            ))
            .style(if index == selected {
                palette.selection()
            } else {
                Style::default()
            }),
            Rect::new(area.x, area.y + 1 + (index - first) as u16, area.width, 1),
        );
    }
    if area.height >= 2 {
        let hint = if selected == 2 && has_draft {
            if area.width < 20 {
                "Draft first Esc"
            } else if area.width < 24 {
                "Finish draft · Esc"
            } else if narrow {
                "Finish draft · Esc back"
            } else {
                "Submit or clear draft · Esc return"
            }
        } else if area.width < 20 {
            "Up/Dn Enter Esc"
        } else if narrow {
            "Up/Dn Enter:open Esc"
        } else {
            "Up/Down · Enter open · Esc return"
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn draw_help_frame(frame: &mut Frame, selected: usize, palette: Palette) {
    let area = bottom_area(frame.area(), 20);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let count = help_len();
    let header = if area.width < 24 {
        format!("Commands {}/{}", selected + 1, count)
    } else {
        format!("Commands · {count} local controls")
    };
    frame.render_widget(
        Paragraph::new(safe_truncate(&header, usize::from(area.width))).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let visible = usize::from(area.height.saturating_sub(3));
    let start = session_picker_start(selected, visible);
    for row in 0..visible {
        let index = start + row;
        let Some(entry) = help_entry(index) else {
            break;
        };
        let marker = if index == selected { "> " } else { "  " };
        let command = if area.width < 24 {
            format!("{marker}/{}", entry.name)
        } else {
            format!(
                "{marker}/{}{}",
                entry.name,
                entry
                    .argument
                    .map_or_else(String::new, |argument| format!(" {argument}"))
            )
        };
        let line = if area.width >= 50 {
            format!("{command} · {}", entry.description)
        } else {
            command
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(&line, usize::from(area.width))).style(
                if index == selected {
                    palette.selection()
                } else {
                    Style::default()
                },
            ),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
    if area.height >= 3 {
        if let Some(entry) = help_entry(selected) {
            frame.render_widget(
                Paragraph::new(safe_truncate(entry.description, usize::from(area.width))),
                Rect::new(area.x, area.bottom() - 2, area.width, 1),
            );
        }
        let hint = if area.width < 24 {
            "Up/Dn Enter Esc".to_owned()
        } else {
            format!("{}/{} · Up/Down · Enter/Esc close", selected + 1, count)
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(&hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn draw_model_catalog_frame(
    frame: &mut Frame,
    picker: &super::ModelPicker<'_>,
    composer: &Composer,
    model: &PresentationModel,
    palette: Palette,
) {
    let full = frame.area();
    if full.height < 8 || full.width < 16 {
        frame.render_widget(Paragraph::new("Resize or Esc").style(palette.error()), full);
        return;
    }
    let draft_height = composer_height(composer, full).min(full.height - 5);
    let composer_area = Rect::new(
        full.x,
        full.bottom() - 1 - draft_height,
        full.width,
        draft_height,
    );
    frame.render_widget(ratatui::widgets::Clear, composer_area);
    draw_composer(frame, model, composer, palette, composer_area, 0, true);
    let available = full.height.saturating_sub(1 + draft_height);
    let height = available.min(12);
    let area = Rect::new(
        full.x,
        full.y + available.saturating_sub(height),
        full.width,
        height,
    );
    if area.width == 0 || area.height < 4 {
        return;
    }
    frame.render_widget(ratatui::widgets::Clear, area);
    let selected = picker.selected;
    let label = picker.items.get(selected).map_or("models", |item| {
        if picker.exact_custom {
            "exact"
        } else if item.runnable {
            "reviewed"
        } else {
            "listed"
        }
    });
    let header = if picker.filter.is_empty() {
        format!("Models · {label} · {} available", picker.items.len())
    } else {
        let suffix = if picker.filter.len() >= 64 {
            " · 64 max"
        } else {
            ""
        };
        format!(
            "Find {}{suffix}",
            model_id_preview(
                picker.filter,
                usize::from(area.width).saturating_sub(5 + suffix.len())
            )
        )
    };
    frame.render_widget(
        Paragraph::new(safe_truncate(&header, usize::from(area.width))).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if picker.catalog_state != super::ModelCatalogState::Current {
        let status = match picker.catalog_state {
            super::ModelCatalogState::Refreshing if full.width >= 50 => {
                "Refreshing models · cached choices remain selectable"
            }
            super::ModelCatalogState::Refreshing => "Refreshing",
            super::ModelCatalogState::Updated => "Refreshed",
            _ if full.width >= 50 => "Refresh failed · cached choices kept · retry /model",
            _ => "Refresh failed",
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(status, usize::from(full.width))).style(
                if picker.catalog_state == super::ModelCatalogState::RefreshFailed {
                    palette.error()
                } else {
                    palette.selection()
                },
            ),
            Rect::new(full.x, full.bottom() - 1, full.width, 1),
        );
    }
    let rows = usize::from(area.height - 3);
    let start = session_picker_start(selected, rows);
    if picker.items.is_empty() {
        frame.render_widget(
            Paragraph::new(if picker.filter.is_empty() {
                "No models available"
            } else {
                "No matches"
            }),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    for (row, item) in picker.items.iter().skip(start).take(rows).enumerate() {
        let index = start + row;
        let focused = index == selected;
        let prefix = format!("{}{} ", if focused { "> " } else { "  " }, index + 1);
        let suffix = if focused {
            format!(
                " <{}>",
                picker.efforts.get(index).copied().flatten().map_or(
                    if area.width < 24 { "def" } else { "default" },
                    crate::provider::Effort::as_str
                )
            )
        } else {
            String::new()
        };
        let id = model_id_preview(
            &item.id,
            usize::from(area.width).saturating_sub(prefix.len() + suffix.len()),
        );
        let line = format!("{prefix}{id}{suffix}");
        frame.render_widget(
            Paragraph::new(safe_truncate(&line, usize::from(area.width))).style(if focused {
                palette.selection()
            } else {
                Style::default()
            }),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
    let navigation = if area.width < 40 {
        "Up/Dn L/R effort"
    } else {
        "Up/Down model · Left/Right reasoning"
    };
    let action = if picker.items.is_empty() {
        "Type/BS · Esc"
    } else if area.width < 40 {
        "Enter pick Esc"
    } else if area.width < 80 {
        "Enter selects both · Esc · Type/BS filter"
    } else {
        "Enter selects both · Esc keeps draft · Type/BS filter · PgUp/PgDn"
    };
    for (offset, text) in [(2, navigation), (1, action)] {
        frame.render_widget(
            Paragraph::new(safe_truncate(text, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - offset, area.width, 1),
        );
    }
}
fn session_picker_start(selected: usize, visible_rows: usize) -> usize {
    selected.saturating_sub(visible_rows.saturating_sub(1))
}

pub(super) fn session_picker_index(
    area: Rect,
    column: u16,
    row: u16,
    selected: usize,
    count: usize,
) -> Option<usize> {
    if count == 0
        || column < area.left()
        || column >= area.right()
        || row <= area.top()
        || row >= area.bottom().saturating_sub(1)
    {
        return None;
    }
    let visible_rows = usize::from(area.height.saturating_sub(2));
    let offset = usize::from(row - area.top() - 1);
    let index = session_picker_start(selected, visible_rows).saturating_add(offset);
    (index < count).then_some(index)
}

pub(super) fn draw_agent_inspector_frame(
    frame: &mut Frame,
    model: &AgentInspectorModel,
    scroll: usize,
    palette: Palette,
) {
    let area = bottom_area(frame.area(), 14);
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(model.lines[0].as_str()).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let choices = agent_choice_rows(area.height, model.choices.len());
    let start = model.selected.saturating_sub(choices.saturating_sub(1));
    for (row, choice) in model.choices.iter().skip(start).take(choices).enumerate() {
        let marker = if start + row == model.selected {
            "> "
        } else {
            "  "
        };
        frame.render_widget(
            Paragraph::new(format!("{marker}{choice}")).style(if start + row == model.selected {
                palette.selection()
            } else {
                Style::default()
            }),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
    let detail_y = area
        .y
        .saturating_add(1 + u16::try_from(choices).unwrap_or(u16::MAX));
    let detail_rows = agent_detail_rows(area.height, model.choices.len());
    for (index, line) in model
        .lines
        .iter()
        .skip(1)
        .skip(scroll)
        .take(detail_rows)
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(safe_truncate(line, usize::from(area.width))),
            Rect::new(area.x, detail_y + index as u16, area.width, 1),
        );
    }
    if area.height > 1 {
        let hint = if area.width < 20 {
            "Esc Up/Dn Pg"
        } else {
            "Esc close · Up/Down agent · PgUp/PgDn"
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn agent_choice_rows(height: u16, count: usize) -> usize {
    usize::from(height / 3).min(4).min(count)
}

pub(super) fn agent_detail_rows(height: u16, count: usize) -> usize {
    usize::from(height.saturating_sub(2)).saturating_sub(agent_choice_rows(height, count))
}

pub(super) fn agent_picker_index(
    area: Rect,
    column: u16,
    row: u16,
    selected: usize,
    count: usize,
) -> Option<usize> {
    let visible = agent_choice_rows(area.height, count);
    if visible == 0
        || column < area.left()
        || column >= area.right()
        || row <= area.top()
        || row
            > area
                .top()
                .saturating_add(u16::try_from(visible).unwrap_or(u16::MAX))
    {
        return None;
    }
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let index = start.saturating_add(usize::from(row - area.top() - 1));
    (index < count).then_some(index)
}

#[cfg(test)]
mod tests;
