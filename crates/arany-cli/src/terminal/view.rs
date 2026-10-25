use super::commands::{Submission, help_entry, help_len};
use super::history::{HistoryRow, MAX_ROW_CELLS, MessageKind, native_marker};
#[cfg(test)]
use super::input::KeyboardSupport;
use super::{Composer, TerminalMode, backend::TerminalBackend, input::TerminalQueries};
use crate::presentation::{
    AgentInspectorModel, DraftAction, MAX_ROW_WIDTH, PresentationModel, model_id_preview,
    safe_truncate,
};
#[cfg(test)]
use arany::ModelEntry;
use arany::SessionListItem;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::Backend,
    buffer::CellDiffOption,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::io;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_VISIBLE_DRAFT_ROWS: usize = 4;

pub(super) struct PermissionFrameState<'a> {
    pub choices: &'a [&'a str],
    pub page: usize,
    pub focus: usize,
    pub profile_start: Option<usize>,
    pub profile_count: usize,
    pub focused_profile: Option<usize>,
    pub profile_selection_enabled: Option<bool>,
    pub footer: Option<&'a str>,
    pub palette: Palette,
    pub boxed: bool,
}

pub(super) struct PermissionFrameProgress {
    pub more: bool,
    pub page: usize,
}

const PROFILE_HANGING_INDENT: usize = 6;

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

pub(super) fn draw_permission_frame(
    frame: &mut Frame,
    full: Rect,
    lines: &[String],
    state: PermissionFrameState<'_>,
) -> PermissionFrameProgress {
    let PermissionFrameState {
        choices,
        page,
        focus,
        profile_start,
        profile_count,
        focused_profile,
        profile_selection_enabled,
        footer,
        palette,
        boxed,
    } = state;
    let gutter = if boxed {
        0
    } else if full.width >= 40 {
        2
    } else {
        0
    };
    let width = if boxed {
        full.width
            .saturating_sub(if full.width >= 40 { 4 } else { 0 })
            .min(76)
    } else {
        full.width.saturating_sub(gutter * 2)
    };
    if width == 0 || full.height < 8 {
        return PermissionFrameProgress { more: true, page };
    }
    let mut rows = Vec::new();
    let mut profile_rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let row_start = rows.len();
        let focused = profile_start
            .zip(focused_profile)
            .is_some_and(|(start, selected)| index == start + selected);
        let profile_row =
            profile_start.is_some_and(|start| (start..start + profile_count).contains(&index));
        let indent = if profile_row {
            PROFILE_HANGING_INDENT
        } else {
            0
        };
        let style = if index == 0 || focused {
            palette.selection()
        } else {
            Style::default()
        };
        for text in wrap_cells(line, usize::from(width), indent) {
            rows.push(Line::styled(text, style));
        }
        if let Some(start) = profile_start
            && profile_row
        {
            profile_rows.push((index - start, row_start));
        }
    }
    let area = if boxed {
        let height = full
            .height
            .min(u16::try_from(rows.len().saturating_add(5)).unwrap_or(u16::MAX))
            .min(20);
        Rect::new(
            full.x + (full.width - width) / 2,
            full.y + (full.height - height) / 2,
            width,
            height,
        )
    } else {
        frame.render_widget(Clear, full);
        Rect::new(full.x + gutter, full.y, width, full.height)
    };
    let mut choice_rows = u16::try_from(choices.len().max(3))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(3));
    let mut body_height = usize::from(area.height - choice_rows - 2);
    // Dropping the spare choice row and spacer keeps a dialog that is two rows over on one page.
    let compact_rows = u16::try_from(choices.len()).unwrap_or(u16::MAX);
    if rows.len() > body_height
        && area.height > compact_rows + 1
        && rows.len() <= usize::from(area.height - compact_rows - 1)
    {
        choice_rows = compact_rows;
        body_height = usize::from(area.height - choice_rows - 1);
    }
    let pages = rows.len().div_ceil(body_height).max(1);
    let page = profile_rows
        .iter()
        .find(|(profile, _)| Some(*profile) == focused_profile)
        .map_or(page.min(pages - 1), |(_, row)| row / body_height);
    let more = page + 1 < pages;
    let start = page * body_height;
    frame.render_widget(
        Paragraph::new(rows[start..rows.len().min(start + body_height)].to_vec()),
        Rect::new(area.x, area.y, width, body_height as u16),
    );
    let options_y = area.bottom() - choice_rows - 1;
    if more {
        let prompt = if width < 24 {
            format!("Page {}/{} Enter", page + 1, pages)
        } else {
            format!("Page {}/{} · Enter/PgDn continues", page + 1, pages)
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(&prompt, usize::from(width))).style(palette.attention()),
            Rect::new(area.x, options_y, width, 1),
        );
    } else {
        for (index, choice) in choices.iter().enumerate() {
            let focused = focus == index;
            let label = format!("{} {}", if focused { "›" } else { " " }, choice);
            frame.render_widget(
                Paragraph::new(safe_truncate(&label, usize::from(width))).style(if focused {
                    palette.selection()
                } else {
                    Style::default()
                }),
                Rect::new(area.x, options_y + index as u16, width, 1),
            );
        }
    }
    let footer = match profile_selection_enabled {
        Some(_) if focused_profile.is_some() => {
            if width < 24 {
                "↑↓ Space · Tab back · ^C"
            } else if width < 50 {
                "↑↓ profiles · Space · Tab back · Ctrl+C"
            } else {
                "↑↓ profiles · Space toggle · Tab back · Ctrl+C exit"
            }
        }
        Some(enabled) if more && !enabled => {
            if width < 50 {
                "Enter/PgDn continues · Ctrl+C exits"
            } else {
                "Enter/PgDn continues; review all pages before profiles · Ctrl+C exits"
            }
        }
        Some(_) if more => {
            if width < 50 {
                "Enter/PgDn · Tab profiles · Ctrl+C exit"
            } else {
                "Enter/PgDn continues · Tab profiles · Ctrl+C exit"
            }
        }
        Some(_) if profile_count > 0 => {
            if width < 24 {
                "↑↓ Tab · Enter · ^C"
            } else if width < 50 {
                "↑↓ choose · Tab profiles · Space · Enter"
            } else {
                "↑↓ choose · Tab profiles · Space toggle · Enter confirm · Ctrl+C exit"
            }
        }
        Some(_) => {
            if width < 24 {
                "↑↓ Tab · Enter · ^C"
            } else if width < 50 {
                "↑↓ choose · Tab next · Enter"
            } else {
                "↑↓ choose · Tab next · Enter confirm · Ctrl+C exit"
            }
        }
        None => footer.unwrap_or_default(),
    };
    frame.render_widget(
        Paragraph::new(safe_truncate(footer, usize::from(width))),
        Rect::new(area.x, area.bottom() - 1, width, 1),
    );
    PermissionFrameProgress { more, page }
}

struct ComposerRows {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_cells: usize,
}

pub(super) fn paste_detail_scroll_limit(text: &str, width: usize, height: usize) -> usize {
    text.split('\n')
        .map(|line| wrap_cells(&crate::presentation::escape_terminal(line), width.max(1), 0).len())
        .sum::<usize>()
        .saturating_sub(height.saturating_sub(2))
}

pub(super) fn draw_paste_detail(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    text: &str,
    scroll: usize,
    palette: Palette,
) {
    let width = usize::from(area.width.max(1));
    let rows: Vec<String> = text
        .split('\n')
        .flat_map(|line| wrap_cells(&crate::presentation::escape_terminal(line), width, 0))
        .collect();
    let visible = usize::from(area.height.saturating_sub(2));
    let first = scroll.min(rows.len().saturating_sub(visible));
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new(safe_truncate(label, width)).style(palette.selection()),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    for (index, row) in rows.iter().skip(first).take(visible).enumerate() {
        frame.render_widget(
            Paragraph::new(row.as_str()),
            Rect::new(area.x, area.y + 1 + index as u16, area.width, 1),
        );
    }
    if area.height > 1 {
        let footer = format!(
            "{}/{} · Up/Down PgUp/PgDn · Esc close",
            first + 1,
            rows.len()
        );
        frame.render_widget(
            Paragraph::new(safe_truncate(&footer, width)),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
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
    let (text, cursor) = composer.display_text();
    let mut cells = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            if offset == cursor {
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
        if offset == cursor {
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
    if cursor == text.len() {
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

pub(super) fn classic_height(
    model: &PresentationModel,
    composer: &Composer,
    screen: Rect,
    live_rows: usize,
) -> u16 {
    let draft = composer_height(composer, screen);
    let menu = completion_height(composer, screen.height);
    let history = if live_rows > 0 {
        let rows = u16::try_from(live_rows).unwrap_or(u16::MAX);
        rows.saturating_add(history_top_padding(rows))
    } else if model.empty_chat && model.activity_lines.is_empty() && menu == 0 {
        4
    } else {
        u16::from(menu > 0)
    };
    draft
        .saturating_add(1)
        .saturating_add(menu)
        .saturating_add(u16::try_from(model.activity_lines.len()).unwrap_or(u16::MAX))
        .saturating_add(history)
        .min(screen.height)
}

pub(super) fn tool_approval_height(
    lines: &[String],
    choices: &[crate::presentation::ToolApprovalChoice],
    width: u16,
) -> u16 {
    let width = usize::from(width.saturating_sub(if width >= 40 { 4 } else { 0 }).max(1));
    let rows = lines
        .iter()
        .map(|line| wrap_cells(line, width, 0).len())
        .sum::<usize>();
    let chrome =
        tool_approval_controls(choices, width, None).len() + 1 + tool_approval_hint_height(width);
    u16::try_from(rows.saturating_add(chrome))
        .unwrap_or(u16::MAX)
        .max(8)
}

fn tool_approval_hint_height(width: usize) -> usize {
    wrap_cells("Review the whole action first (PgDn)", width, 0)
        .len()
        .max(wrap_cells("1-3 choose · ↑↓ Enter · Esc = No", width, 0).len())
        .max(wrap_cells("↓ 99999999 more lines · PgDn", width, 0).len())
}

fn tool_approval_controls(
    choices: &[crate::presentation::ToolApprovalChoice],
    width: usize,
    focus: Option<usize>,
) -> Vec<Line<'static>> {
    let mut rows = wrap_cells("Do you want to proceed?", width, 0)
        .into_iter()
        .map(Line::raw)
        .collect::<Vec<_>>();
    for (index, choice) in choices.iter().enumerate() {
        let style = choice_style(focus, index);
        rows.extend(
            wrap_cells(
                &numbered_label(index, choice.label(), focus == Some(index)),
                width,
                5,
            )
            .into_iter()
            .map(|row| Line::styled(row, style)),
        );
    }
    rows
}

fn numbered_label(index: usize, label: &str, focused: bool) -> String {
    format!("{} {}. {label}", if focused { "›" } else { " " }, index + 1)
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

pub(super) fn draw_tool_approval_frame(
    frame: &mut Frame,
    area: Rect,
    lines: &[String],
    choices: &[crate::presentation::ToolApprovalChoice],
    page: usize,
    focus: Option<usize>,
    review_notice: bool,
) -> bool {
    frame.render_widget(Clear, area);
    let gutter = u16::from(area.width >= 40) * 2;
    let area = Rect::new(
        area.x + gutter,
        area.y,
        area.width.saturating_sub(gutter * 2),
        area.height,
    );
    let width = usize::from(area.width.max(1));
    let controls = tool_approval_controls(choices, width, focus);
    let hint_height = tool_approval_hint_height(width);
    let chrome = controls.len().saturating_add(1 + hint_height);
    if area.width < 16 || usize::from(area.height) <= chrome {
        frame.render_widget(
            Paragraph::new("Resize to review; Esc denies").wrap(Wrap { trim: false }),
            area,
        );
        return true;
    }
    let rows = lines
        .iter()
        .flat_map(|line| wrap_cells(line, width, 0))
        .map(Line::raw)
        .collect::<Vec<_>>();
    let body_height = usize::from(area.height) - chrome;
    let pages = rows.len().div_ceil(body_height).max(1);
    let page = page.min(pages - 1);
    let start = page * body_height;
    let end = rows.len().min(start + body_height);
    frame.render_widget(
        Paragraph::new("Tool approval").style(Style::default().add_modifier(Modifier::BOLD)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(rows[start..end].to_vec()),
        Rect::new(area.x, area.y + 1, area.width, body_height as u16),
    );
    let more = page + 1 < pages;
    let hint = if review_notice {
        "Review the whole action first (PgDn)".to_owned()
    } else if more {
        format!("↓ {} more lines · PgDn", rows.len() - end)
    } else {
        format!("1-{} choose · ↑↓ Enter · Esc = No", choices.len())
    };
    let controls_y = area.bottom() - controls.len() as u16;
    frame.render_widget(
        Paragraph::new(
            wrap_cells(&hint, width, 0)
                .into_iter()
                .map(Line::raw)
                .collect::<Vec<_>>(),
        ),
        Rect::new(
            area.x,
            controls_y - hint_height as u16,
            area.width,
            hint_height as u16,
        ),
    );
    frame.render_widget(
        Paragraph::new(controls),
        Rect::new(
            area.x,
            controls_y,
            area.width,
            controls_y.abs_diff(area.bottom()),
        ),
    );
    more
}

pub(super) fn permission_height(
    lines: &[String],
    width: u16,
    boxed: bool,
    profile_start: Option<usize>,
) -> u16 {
    let width = if boxed {
        width
            .saturating_sub(if width >= 40 { 4 } else { 0 })
            .min(76)
    } else {
        width.saturating_sub(if width >= 40 { 4 } else { 0 })
    };
    let rows = lines
        .iter()
        .enumerate()
        .fold(0usize, |rows, (index, line)| {
            let indent = if profile_start.is_some_and(|start| index >= start) {
                PROFILE_HANGING_INDENT
            } else {
                0
            };
            rows.saturating_add(wrap_cells(line, usize::from(width.max(1)), indent).len())
        });
    let height = u16::try_from(rows.saturating_add(5))
        .unwrap_or(u16::MAX)
        .max(8);
    if boxed { height.min(20) } else { height }
}

pub(super) fn history_top_padding(available_height: u16) -> u16 {
    u16::from(available_height > 4)
}

pub(super) fn completion_height(composer: &Composer, available_height: u16) -> u16 {
    composer.completion_rows().map_or(0, |(names, _)| {
        let height = if composer.completion_header().is_some() {
            names.len().clamp(1, 20) as u16 + 1
        } else {
            names.len().min(3) as u16
        };
        height.min(available_height.saturating_sub(1))
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

#[expect(
    clippy::too_many_arguments,
    reason = "dialog content and its explicit rendering bounds stay together"
)]
pub(super) fn draw_setup_frame(
    frame: &mut Frame,
    full: Rect,
    step: u8,
    title: &str,
    instruction: &str,
    input_chars: usize,
    notice: Option<&str>,
    palette: Palette,
) {
    let area = setup_area(full);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let waiting = matches!(step, 0 | 4 | 6);
    let narrow_input = area.width < 24 && !waiting;
    let status = if waiting {
        "Please wait".to_owned()
    } else if narrow_input {
        format!("{input_chars} chars")
    } else if area.width < 50 {
        format!("{input_chars} chars · Enter · Esc")
    } else {
        format!("Input: {input_chars} characters · Enter confirms · Esc cancels")
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
            Paragraph::new(safe_truncate(instruction, MAX_ROW_WIDTH)).wrap(Wrap { trim: false }),
            Rect::new(
                area.x,
                area.y + 2,
                area.width,
                area.height - 4 - u16::from(narrow_input),
            ),
        );
        frame.render_widget(
            Paragraph::new(safe_truncate(&status, usize::from(area.width))),
            Rect::new(
                area.x,
                area.bottom() - 2 - u16::from(narrow_input),
                area.width,
                1,
            ),
        );
        if narrow_input {
            frame.render_widget(
                Paragraph::new("Enter · Esc"),
                Rect::new(area.x, area.bottom() - 2, area.width, 1),
            );
        }
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
    full: Rect,
    title: &str,
    instruction: &str,
    draft: &Composer,
    notice: Option<&str>,
    palette: Palette,
) {
    let area = setup_area(full);
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

#[expect(
    clippy::too_many_arguments,
    reason = "dialog content and its explicit rendering bounds stay together"
)]
pub(super) fn draw_setup_choices_frame(
    frame: &mut Frame,
    full: Rect,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    selected: usize,
    notice: Option<&str>,
    palette: Palette,
) {
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
    let notice = notice.filter(|_| area.height > 3);
    let compact = area.width < 20;
    let instruction = safe_truncate(instruction, MAX_ROW_WIDTH);
    let supporting_rows = if instruction.width() <= usize::from(area.width) {
        4
    } else {
        5
    };
    let dense = choices.len() > usize::from(area.height.saturating_sub(supporting_rows));
    let header = if compact {
        "↑↓ Enter Esc"
    } else if dense {
        "↑↓ ←→ Enter Esc"
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
        usize::from(area.height.saturating_sub(2 + u16::from(notice.is_some())))
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
    }
    if !dense || notice.is_some() {
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

pub(super) fn setup_warning_height(warning: &str, width: u16) -> u16 {
    u16::try_from(
        wrap_setup_warning(warning, usize::from(width.clamp(1, 72)))
            .len()
            .saturating_add(5),
    )
    .unwrap_or(u16::MAX)
    .clamp(8, 16)
}

#[expect(
    clippy::too_many_arguments,
    reason = "dialog content and its explicit rendering bounds stay together"
)]
pub(super) fn draw_setup_warning_frame(
    frame: &mut Frame,
    full: Rect,
    title: &str,
    warning: &str,
    page: usize,
    accept_selected: bool,
    notice: Option<&str>,
    palette: Palette,
) -> bool {
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
    let body_height = height - 5;
    let lines = wrap_setup_warning(warning, usize::from(width));
    let pages = lines.len().div_ceil(usize::from(body_height));
    let page = page.min(pages - 1);
    let has_more = page + 1 < pages;
    frame.render_widget(
        Paragraph::new(Line::styled(
            safe_truncate(title, usize::from(width)),
            palette.selection(),
        )),
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
    if has_more {
        frame.render_widget(
            Paragraph::new(safe_truncate(&prompt, usize::from(width))),
            Rect::new(area.x, area.bottom() - 4, width, 1),
        );
    } else {
        for (index, label) in ["Back", "Accept"].iter().enumerate() {
            let focused = accept_selected == (index == 1);
            frame.render_widget(
                Paragraph::new(format!("{} {label}", if focused { "›" } else { " " })).style(
                    if focused {
                        palette.selection()
                    } else {
                        Style::default()
                    },
                ),
                Rect::new(area.x, area.bottom() - 4 + index as u16, width, 1),
            );
        }
    }
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

fn wrap_setup_warning(warning: &str, width: usize) -> Vec<String> {
    let normalized = warning.split_whitespace().collect::<Vec<_>>().join(" ");
    wrap_cells(&normalized, width, 0)
}

#[cfg(test)]
pub(super) fn draw_frame(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    notice: Option<&str>,
    palette: Palette,
) {
    draw_frame_with_history(
        frame,
        model,
        composer,
        notice,
        ComposerHints::new(palette, composer.newline_hint(KeyboardSupport::Legacy)),
        &[],
        None,
    );
}

pub(super) fn draw_history_rows(
    frame: &mut Frame,
    area: Rect,
    history_rows: &[HistoryRow],
    palette: Palette,
) {
    for (row, line) in history_rows
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let style = if line.matched {
            palette.selection()
        } else if line.speaker == MessageKind::Supervisor {
            Style::default().add_modifier(Modifier::DIM)
        } else if line.heading {
            match line.speaker {
                MessageKind::User | MessageKind::Assignment => {
                    Style::default().add_modifier(Modifier::BOLD)
                }
                MessageKind::Assistant
                | MessageKind::AgentResult
                | MessageKind::Preview
                | MessageKind::Incomplete => palette.selection(),
                MessageKind::Tool | MessageKind::ToolDetails | MessageKind::Commentary => {
                    palette.accent()
                }
                MessageKind::Supervisor => Style::default().add_modifier(Modifier::DIM),
                MessageKind::Notice | MessageKind::SteeringNote => {
                    palette.accent().add_modifier(Modifier::BOLD)
                }
                MessageKind::Error => palette.error().add_modifier(Modifier::BOLD),
            }
        } else {
            Style::default()
        };
        let marker = match (line.speaker, line.heading) {
            (MessageKind::User | MessageKind::Assignment, true) => Some("› "),
            (MessageKind::Assistant | MessageKind::AgentResult, true) => Some("● "),
            (MessageKind::User | MessageKind::Assignment, false) if !line.text.is_empty() => {
                Some("│ ")
            }
            _ => None,
        };
        let spans = if let Some(marker) = marker {
            let width = usize::from(area.width).min(MAX_ROW_CELLS);
            let text = if line.heading {
                line.text.as_str()
            } else {
                line.text.strip_prefix("  ").unwrap_or(&line.text)
            };
            let mut spans = vec![Span::styled(safe_truncate(marker, width), palette.accent())];
            spans.extend(slash_spans(
                safe_truncate(text, width.saturating_sub(2)),
                line.slash_cells,
                style,
                palette,
            ));
            spans
        } else {
            vec![Span::styled(line.text.as_str(), style)]
        };
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x, area.y + row as u16, area.width, 1),
        );
        // Inverse video marks a selection without relying on color.
        if let Some((from, to)) = line.selected {
            let from = u16::try_from(from).unwrap_or(u16::MAX).min(area.width);
            let to = u16::try_from(to).unwrap_or(u16::MAX).min(area.width);
            frame.buffer_mut().set_style(
                Rect::new(
                    area.x + from,
                    area.y + row as u16,
                    to.saturating_sub(from),
                    1,
                ),
                Style::default().add_modifier(Modifier::REVERSED),
            );
        }
    }
}

// The transcript pager: History above one footer row of keys, the find prompt or a transient cue.
pub(super) fn draw_pager_frame(
    frame: &mut Frame,
    history_rows: &[HistoryRow],
    footer: &str,
    palette: Palette,
) -> Rect {
    let area = frame.area();
    let history_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    draw_history_rows(frame, history_area, history_rows, palette);
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new(accent_lead(
                safe_truncate(footer, usize::from(area.width)),
                palette,
            )),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    history_area
}

#[cfg(test)]
pub(super) fn draw_frame_with_history(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    notice: Option<&str>,
    hints: ComposerHints<'_>,
    history_rows: &[HistoryRow],
    history_status: Option<&str>,
) -> Rect {
    draw_frame_with_history_and_chips(
        frame,
        model,
        composer,
        notice,
        hints,
        history_rows,
        history_status,
    )
    .history_area
}

pub(super) struct FrameGeometry {
    pub history_area: Rect,
    pub image_chips: Vec<(usize, Rect)>,
}

pub(super) fn draw_frame_with_history_and_chips(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    notice: Option<&str>,
    hints: ComposerHints<'_>,
    history_rows: &[HistoryRow],
    history_status: Option<&str>,
) -> FrameGeometry {
    let palette = hints.palette;
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return FrameGeometry {
            history_area: Rect::default(),
            image_chips: Vec::new(),
        };
    }
    let draft_rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    let composer_height = (draft_rows.lines.len().min(MAX_VISIBLE_DRAFT_ROWS) as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(1));
    let menu_height = completion_height(composer, area.height.saturating_sub(1 + composer_height));
    let menu_y = area.bottom().saturating_sub(1 + menu_height);
    let composer_y = menu_y.saturating_sub(composer_height);
    let activity_rows = composer_y
        .saturating_sub(area.y)
        .min(u16::try_from(model.activity_lines.len()).unwrap_or(u16::MAX));
    let available_history = composer_y.saturating_sub(area.y + activity_rows);
    let padding = history_top_padding(available_history);
    let history_top = area.y + padding;
    let history_height = available_history - padding;
    let history_area = Rect::new(area.x, history_top, area.width, history_height);
    draw_history_rows(frame, history_area, history_rows, palette);
    if history_rows.is_empty()
        && history_status.is_none()
        && model.empty_chat
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
                "/help commands · Ctrl+K actions · \\ then Enter newline",
            ]
        };
        let top = history_top + history_height.saturating_sub(4);
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
            Rect::new(area.x, composer_y - activity_rows + row, area.width, 1),
        );
    }
    if menu_height > 0
        && let Some((names, selected)) = composer.completion_rows()
    {
        let header = composer.completion_header();
        let header_rows = u16::from(header.is_some());
        let rows = usize::from(menu_height.saturating_sub(header_rows));
        let start = selected
            .saturating_sub(rows / 2)
            .min(names.len().saturating_sub(rows));
        if let Some(header) = header {
            let header = if area.width < 40 {
                match header {
                    "[Commands]  Skills · Left/Right" => "[Cmd] Skills ↔",
                    "Commands  [Skills] · Left/Right" => "Cmd [Skills] ↔",
                    _ => "Commands + Skills",
                }
            } else {
                header
            };
            let range = if names.len() > rows {
                format!(
                    " · {}-{}/{}",
                    start + 1,
                    (start + rows).min(names.len()),
                    names.len()
                )
            } else {
                String::new()
            };
            let header_line = if composer.completion_tabs() {
                let commands = if area.width < 40 { "Cmd" } else { "Commands" };
                let selected = palette.selection().add_modifier(Modifier::REVERSED);
                let skills_selected = composer.completion_skills_selected();
                let tabs = [(commands, !skills_selected), ("Skills", skills_selected)];
                let mut spans = Vec::new();
                for (index, (label, focused)) in tabs.into_iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        if focused {
                            format!("[{label}]")
                        } else {
                            label.to_owned()
                        },
                        if focused { selected } else { palette.subtle() },
                    ));
                }
                spans.push(Span::styled(
                    if area.width < 40 {
                        " ↔"
                    } else {
                        " · Left/Right"
                    },
                    palette.subtle(),
                ));
                spans.push(Span::styled(range, palette.subtle()));
                Line::from(spans)
            } else {
                Line::styled(
                    safe_truncate(&format!("{header}{range}"), usize::from(area.width)),
                    palette.accent(),
                )
            };
            frame.render_widget(
                Paragraph::new(header_line),
                Rect::new(area.x, menu_y, area.width, 1),
            );
            if names.is_empty() && rows > 0 {
                frame.render_widget(
                    Paragraph::new(safe_truncate(
                        composer.empty_completion_notice(),
                        usize::from(area.width),
                    ))
                    .style(palette.subtle()),
                    Rect::new(area.x, menu_y + 1, area.width, 1),
                );
            }
        }
        for (row, name) in names.iter().enumerate().skip(start).take(rows) {
            let focused = row == selected;
            let marker = if focused { "> " } else { "  " };
            let line = if header.is_some() && area.width >= 40 {
                let name_width = usize::from(area.width / 3).clamp(18, 32);
                let label = safe_truncate(name, name_width);
                let padding = name_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
                let description = composer.completion_description(name);
                format!("{marker}{label}{}  {description}", " ".repeat(padding))
            } else {
                format!("{marker}{name}")
            };
            frame.render_widget(
                Paragraph::new(safe_truncate(&line, usize::from(area.width))).style(if focused {
                    palette.selection()
                } else {
                    Style::default()
                }),
                Rect::new(
                    area.x,
                    menu_y + header_rows + (row - start) as u16,
                    area.width,
                    1,
                ),
            );
        }
    }
    let composer_area = Rect {
        y: composer_y,
        height: composer_height,
        ..area
    };
    let image_chips = draw_composer(
        frame,
        model,
        composer,
        composer_area,
        ComposerHints {
            menu_height,
            ..hints
        },
    );
    let preview = (!composer.text().contains('\n'))
        .then(|| composer.command_preview(model.draft_action == DraftAction::InspectRun))
        .flatten();
    let status_y = area.bottom() - 1;
    if status_y < area.bottom() {
        // The state leads; the history cue keeps its room even when the state is long.
        let wide_history_status = history_status.filter(|_| area.width >= 80).map(|status| {
            let separator = UnicodeWidthStr::width(" · ");
            let state_room =
                usize::from(area.width).saturating_sub(UnicodeWidthStr::width(status) + separator);
            let state = safe_truncate(&model.status_line, state_room);
            if state.is_empty() {
                status.to_owned()
            } else {
                format!("{state} · {status}")
            }
        });
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
    FrameGeometry {
        history_area,
        image_chips,
    }
}

pub(super) fn draw_image_preview(frame: &mut Frame, area: Rect, title: &str) {
    frame.render_widget(Clear, area);
    let title = safe_truncate(title, usize::from(area.width.saturating_sub(4)));
    frame.render_widget(Block::bordered().title(title), area);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    for row in inner.y..inner.bottom() {
        for column in inner.x..inner.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut(Position::new(column, row)) {
                cell.set_style(Style::default().add_modifier(Modifier::HIDDEN));
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

pub(super) fn native_line(row: &HistoryRow, width: u16, palette: Palette) -> Line<'static> {
    let quoted = matches!(row.speaker, MessageKind::User | MessageKind::Assignment);
    let marker = if quoted && !row.heading && row.text.is_empty() {
        ""
    } else {
        native_marker(row.speaker, row.heading)
    };
    let text = if quoted && !row.heading {
        row.text.strip_prefix("  ").unwrap_or(&row.text)
    } else {
        &row.text
    };
    let mut spans = vec![Span::styled(marker, palette.accent())];
    spans.extend(slash_spans(
        safe_truncate(text, usize::from(width).saturating_sub(marker.width())),
        row.slash_cells,
        if row.heading {
            palette.selection()
        } else {
            Style::default()
        },
        palette,
    ));
    Line::from(spans)
}

pub(super) fn draw_session_picker_frame(
    frame: &mut Frame,
    area: Rect,
    items: &[SessionListItem],
    selected: usize,
    armed: bool,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let header = format!("Resume · {} in this Workspace", items.len());
    frame.render_widget(
        Paragraph::new(safe_truncate(&header, usize::from(area.width))).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rows = usize::from(area.height.saturating_sub(6));
    let start = session_picker_start(selected, rows);
    for (row, item) in items.iter().enumerate().skip(start).take(rows) {
        frame.render_widget(
            Paragraph::new(numbered_line(
                row,
                &item.title,
                armed.then_some(selected),
                area.width,
            )),
            Rect::new(area.x, area.y + 1 + (row - start) as u16, area.width, 1),
        );
    }
    if area.height >= 6
        && let Some(item) = items.get(selected)
    {
        let short = area.width < 40;
        let created = if short {
            item.created_at.get(..10).unwrap_or(&item.created_at)
        } else {
            &item.created_at
        };
        let used = if short {
            item.last_activity_at
                .get(..10)
                .unwrap_or(&item.last_activity_at)
        } else {
            &item.last_activity_at
        };
        let details = [
            if short {
                format!("Made {created}")
            } else {
                format!("Created: {created} UTC")
            },
            if short {
                format!("Used {used}")
            } else {
                format!("Last activity: {used} UTC")
            },
            crate::presentation::session_access_label(&item.defaults).to_owned(),
            item.defaults
                .model
                .clone()
                .unwrap_or_else(|| "Default model".into()),
        ];
        for (offset, detail) in details.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(safe_truncate(detail, usize::from(area.width))),
                Rect::new(area.x, area.bottom() - 5 + offset as u16, area.width, 1),
            );
        }
    }
    if armed && area.height >= 2 {
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

#[derive(Clone, Copy)]
pub(super) struct ComposerHints<'a> {
    palette: Palette,
    menu_height: u16,
    picker_focused: bool,
    newline: &'a str,
}

impl<'a> ComposerHints<'a> {
    pub(super) fn new(palette: Palette, newline: &'a str) -> Self {
        Self {
            palette,
            menu_height: 0,
            picker_focused: false,
            newline,
        }
    }
}

fn draw_composer(
    frame: &mut Frame,
    model: &PresentationModel,
    composer: &Composer,
    composer_area: Rect,
    hints: ComposerHints<'_>,
) -> Vec<(usize, Rect)> {
    let ComposerHints {
        palette,
        menu_height,
        picker_focused,
        newline,
    } = hints;
    let area = composer_area;
    let draft_rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    let draft = composer.text();
    let visible_lines = usize::from(composer_area.height.saturating_sub(2));
    let first_line = draft_rows
        .cursor_row
        .saturating_sub(visible_lines / 2)
        .min(draft_rows.lines.len().saturating_sub(visible_lines));
    let preview = (!picker_focused && !draft.contains('\n'))
        .then(|| composer.command_preview(model.draft_action == DraftAction::InspectRun))
        .flatten();
    let inner_width = usize::from(area.width.saturating_sub(2));
    let mut rendered_lines = Vec::with_capacity(visible_lines);
    let mut slash_cells = composer
        .slash_token()
        .map_or(0, |token| token.text.len())
        .saturating_sub(draft_rows.lines[..first_line].iter().map(String::len).sum());
    for (index, line) in draft_rows
        .lines
        .iter()
        .enumerate()
        .skip(first_line)
        .take(visible_lines)
    {
        let mut spans = slash_spans(
            line.clone(),
            slash_cells.min(line.len()),
            Style::default(),
            palette,
        );
        slash_cells = slash_cells.saturating_sub(line.len());
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
            spans.push(Span::styled(
                ghost,
                if palette.color {
                    Style::default().add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                },
            ));
        }
        rendered_lines.push(Line::from(spans));
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
    let mut compact_image_chip = !composer.images().is_empty() && composer_area.width < 28;
    if !composer.images().is_empty() {
        let labels = (0..composer.images().len())
            .map(|index| {
                format!(
                    "{}[Image #{}]",
                    if composer.selected_image() == Some(index) {
                        ">"
                    } else {
                        ""
                    },
                    index + 1
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let all_chips = format!(" · {labels}{position}");
        compact_image_chip |= UnicodeWidthStr::width(all_chips.as_str())
            > usize::from(composer_area.width).saturating_sub(12);
        if compact_image_chip {
            let chip = match (composer.selected_image(), composer.images().len()) {
                (Some(index), _) => format!(" >[Image #{}]", index + 1),
                (None, 1) => " [Image #1]".to_owned(),
                (None, count) => format!(" [{count} images]"),
            };
            position = if UnicodeWidthStr::width(chip.as_str())
                + UnicodeWidthStr::width(position.as_str())
                <= usize::from(composer_area.width)
            {
                format!("{chip}{position}")
            } else {
                chip
            };
        } else {
            position = all_chips;
        }
    }
    let title = if compact_image_chip {
        ""
    } else if 2 + "Ask Arany".len() + position.len() <= usize::from(composer_area.width) {
        "Ask Arany"
    } else {
        "Draft"
    };
    let mut image_chips = Vec::new();
    if !compact_image_chip {
        let lead_width = if title == "Ask Arany" { 2 } else { 0 };
        let mut x = area.x.saturating_add(lead_width + title.len() as u16 + 3);
        for index in 0..composer.images().len() {
            x = x.saturating_add(u16::from(composer.selected_image() == Some(index)));
            let width = format!("[Image #{}]", index + 1).len() as u16;
            image_chips.push((index, Rect::new(x, area.y, width, 1)));
            x = x.saturating_add(width + 1);
        }
    } else if let Some(index) = composer.selected_image() {
        let x = area.x.saturating_add(2);
        image_chips.push((index, Rect::new(x, area.y, "[Image #0]".len() as u16, 1)));
    } else if composer.images().len() == 1 {
        image_chips.push((
            0,
            Rect::new(area.x + 1, area.y, "[Image #1]".len() as u16, 1),
        ));
    }
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
            && matches!(composer.submission(), Ok(Submission::Command { .. }));
        let hint = if composer.selected_image().is_some() {
            let image_hint = " ←/→ select · Del/Backspace remove ";
            if usize::from(composer_area.width) >= UnicodeWidthStr::width(image_hint) {
                image_hint
            } else {
                " Del remove "
            }
        } else if composer.selected_paste().is_some() {
            if composer_area.width >= 28 {
                " Tab: inspect · Del: remove "
            } else {
                " Tab / Del "
            }
        } else if picker_focused {
            if composer_area.width < 40 {
                " Draft kept "
            } else {
                " Draft kept · Esc returns "
            }
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 40 {
            "Enter keeps draft · \\ then Enter newline"
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 24 {
            " Enter keeps · \\ Enter "
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 16 {
            " Enter keeps "
        } else if menu_height > 0 && composer_area.width >= 40 {
            if draft.starts_with('/') {
                " Tab completes · Enter invokes · Esc close "
            } else {
                " Up/Down · Tab/Enter fills · Esc close "
            }
        } else if menu_height > 0 && composer_area.width >= 20 {
            if draft.starts_with('/') {
                " Tab fills Enter runs "
            } else {
                " Tab/Enter fills Esc "
            }
        } else if menu_height > 0 {
            if draft.starts_with('/') {
                " Enter runs Esc "
            } else {
                " Tab/Enter Esc "
            }
        } else if command && composer_area.width >= 40 {
            " Enter command · \\ then Enter newline "
        } else if command && composer_area.width >= 16 {
            " Enter command "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 40 {
            "Enter keeps draft · \\ then Enter newline"
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 24 {
            " Enter keeps · \\ Enter "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 16 {
            " Enter keeps "
        } else if composer_area.width >= 40 {
            " Enter sends · \\ then Enter newline "
        } else if composer_area.width >= 16 {
            " Enter · \\ Enter "
        } else {
            ""
        };
        let agents_hint = draft.is_empty()
            && composer.images().is_empty()
            && model.agent_switch
            && menu_height == 0
            && !picker_focused;
        let hint = match composer_area.width {
            40.. if agents_hint => " Alt+Left/Right agents ",
            24.. if agents_hint => " Alt+←/→ agents ",
            _ => hint,
        };
        let hint = if composer_area.width >= 40 && newline != "\\ then Enter" {
            hint.replace("\\ then Enter newline", &format!("{} / \\ Enter", newline))
                .replace("Enter keeps draft", "Enter keeps")
        } else {
            hint.to_owned()
        };
        let rules =
            usize::from(composer_area.width).saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("─".repeat(rules / 2), palette.subtle()),
                Span::raw(hint),
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
    image_chips
}

pub(super) fn draw_quick_actions_frame(
    frame: &mut Frame,
    area: Rect,
    selected: usize,
    armed: bool,
    has_draft: bool,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new("Quick actions").style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let labels = if area.width < 24 {
        ["Models", "Agents", "Resume", "Setup", "Usage"]
    } else if area.width < 40 {
        [
            "Models · choose",
            "Agents · inspect",
            if has_draft {
                "Resume · draft"
            } else {
                "Resume · choose"
            },
            "Setup · account",
            "Usage · inspect",
        ]
    } else {
        [
            "Models · choose a model",
            "Agents · inspect work",
            if has_draft {
                "Resume · finish draft first"
            } else {
                "Resume · conversation"
            },
            "Setup · connect an account",
            "Usage · reported tokens",
        ]
    };
    let visible = usize::from(area.height.saturating_sub(2));
    let first = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(labels.len().saturating_sub(visible));
    for (index, label) in labels.into_iter().enumerate().skip(first).take(visible) {
        frame.render_widget(
            Paragraph::new(numbered_line(
                index,
                label,
                armed.then_some(selected),
                area.width,
            )),
            Rect::new(area.x, area.y + 1 + (index - first) as u16, area.width, 1),
        );
    }
    if armed && area.height >= 2 {
        let hint = if selected == 2 && has_draft {
            if area.width < 20 {
                "Draft first Esc"
            } else if area.width < 24 {
                "Submit draft · Esc"
            } else if area.width < 40 {
                "Submit draft · Esc back"
            } else {
                "Submit or clear draft · Esc return"
            }
        } else if area.width < 20 {
            "1-5 Enter Esc"
        } else if area.width < 40 {
            "1-5 Enter:open Esc"
        } else {
            "1-5 or Up/Down · Enter open · Esc return"
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

// A row too wide for one line puts its value on the next, so a change stays visible.
fn settings_stacked(rows: &[String], width: usize) -> bool {
    rows.iter()
        .enumerate()
        .any(|(index, row)| numbered_label(index, row, false).width() > width)
}

// Classic selector lists show at most this many rows and scroll inside, like Codex's
// MAX_POPUP_ROWS; ten matches the page size of the linear pickers.
pub(super) const SELECTOR_ROWS: usize = 10;

pub(super) fn settings_height(rows: &[String], width: u16, max_rows: usize) -> u16 {
    let lines_per_row = if settings_stacked(rows, usize::from(width)) {
        2
    } else {
        1
    };
    u16::try_from(rows.len().min(max_rows) * lines_per_row + 2).unwrap_or(u16::MAX)
}

pub(super) fn guidance_height(text: &str, width: u16) -> u16 {
    u16::try_from(wrap_cells(text, usize::from(width.max(1)), 0).len()).unwrap_or(u16::MAX)
}

pub(super) fn draw_settings_frame(
    frame: &mut Frame,
    area: Rect,
    rows: &[String],
    selected: usize,
    armed: bool,
    notice: Option<&str>,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new("Settings").style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let width = usize::from(area.width);
    let focus = armed.then_some(selected);
    let stacked = settings_stacked(rows, width);
    let lines_per_row = if stacked { 2 } else { 1 };
    let guidance = notice
        .map(|text| wrap_cells(text, width.max(1), 0))
        .unwrap_or_default();
    let guidance_rows = guidance
        .len()
        .min(usize::from(area.height.saturating_sub(2 + lines_per_row)));
    let visible =
        (usize::from(area.height.saturating_sub(2)) - guidance_rows) / usize::from(lines_per_row);
    let start = session_picker_start(selected, visible);
    for (index, row) in rows.iter().enumerate().skip(start).take(visible) {
        let y = area.y + 1 + ((index - start) * usize::from(lines_per_row)) as u16;
        let (label, value) = match row.split_once(": ") {
            Some((name, value)) if stacked => (name, Some(value)),
            _ => (row.as_str(), None),
        };
        frame.render_widget(
            Paragraph::new(numbered_line(index, label, focus, area.width)),
            Rect::new(area.x, y, area.width, 1),
        );
        if let Some(value) = value {
            let indent = 5.min(width.saturating_sub(value.width()));
            frame.render_widget(
                Paragraph::new(Line::styled(
                    safe_truncate(&format!("{}{value}", " ".repeat(indent)), width),
                    choice_style(focus, index),
                )),
                Rect::new(area.x, y + 1, area.width, 1),
            );
        }
    }
    // The key hint appears once armed, telling the user that input now counts.
    if area.height >= 2 && (armed || notice.is_some()) {
        for (index, line) in guidance.iter().take(guidance_rows).enumerate() {
            frame.render_widget(
                Paragraph::new(line.as_str()).style(palette.attention()),
                Rect::new(
                    area.x,
                    area.bottom() - 1 - guidance_rows as u16 + index as u16,
                    area.width,
                    1,
                ),
            );
        }
        let (footer, style) = match area.width {
            0..50 => ("↑↓ ←→ Enter Esc", Style::default()),
            _ => (
                "1-9 or ↑↓ select · ←→ change · Enter saves · Esc",
                Style::default(),
            ),
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(footer, usize::from(area.width))).style(style),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn usage_lines(page: &str, width: u16) -> Vec<String> {
    page.lines()
        .flat_map(|line| wrap_cells(line, usize::from(width).max(1), 0))
        .collect()
}

pub(super) fn draw_usage_frame(
    frame: &mut Frame,
    area: Rect,
    lines: &[String],
    scroll: usize,
    page: (usize, usize),
    armed: bool,
    palette: Palette,
) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let header = format!("Usage {}/{}", page.0 + 1, page.1);
    frame.render_widget(
        Paragraph::new(header).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let visible = usize::from(area.height.saturating_sub(2));
    let start = scroll.min(lines.len().saturating_sub(visible));
    for (index, line) in lines.iter().skip(start).take(visible).enumerate() {
        frame.render_widget(
            Paragraph::new(line.as_str()),
            Rect::new(area.x, area.y + 1 + index as u16, area.width, 1),
        );
    }
    if armed && area.height > 1 {
        let hint = if area.width < 40 {
            "↑↓ ←→ Esc"
        } else {
            "Up/Down · Left/Right page · Esc close"
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(hint, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) fn draw_help_frame(
    frame: &mut Frame,
    area: Rect,
    selected: usize,
    armed: bool,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
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
    let description = help_entry(selected)
        .map(|entry| wrap_cells(entry.description, usize::from(area.width), 0))
        .unwrap_or_default();
    let description_rows = description
        .len()
        .min(usize::from(area.height.saturating_sub(3)));
    let visible = usize::from(area.height.saturating_sub(2)) - description_rows;
    let start = session_picker_start(selected, visible);
    for row in 0..visible {
        let index = start + row;
        let Some(entry) = help_entry(index) else {
            break;
        };
        let command = if area.width < 24 {
            format!("/{}", entry.name)
        } else {
            format!(
                "/{}{}",
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
            Paragraph::new(numbered_line(
                index,
                &line,
                armed.then_some(selected),
                area.width,
            )),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
    if area.height >= 3 {
        for (index, line) in description.iter().take(description_rows).enumerate() {
            frame.render_widget(
                Paragraph::new(line.as_str()),
                Rect::new(
                    area.x,
                    area.bottom() - 1 - description_rows as u16 + index as u16,
                    area.width,
                    1,
                ),
            );
        }
        let hint = if area.width < 24 {
            "Up/Dn Enter Esc".to_owned()
        } else {
            format!("{}/{} · Up/Down · Enter/Esc close", selected + 1, count)
        };
        if armed {
            frame.render_widget(
                Paragraph::new(safe_truncate(&hint, usize::from(area.width))),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        }
    }
}

pub(super) fn draw_model_catalog_frame(
    frame: &mut Frame,
    picker: &super::ModelPicker<'_>,
    armed: bool,
    composer: &Composer,
    model: &PresentationModel,
    hints: ComposerHints<'_>,
) {
    let palette = hints.palette;
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
    draw_composer(
        frame,
        model,
        composer,
        composer_area,
        ComposerHints {
            picker_focused: true,
            ..hints
        },
    );
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
        let focused = armed && index == selected;
        let prefix = numbered_label(index, "", focused).width();
        let suffix = if focused {
            format!(
                " <{}>",
                picker.efforts.get(index).copied().flatten().map_or(
                    if area.width < 24 { "def" } else { "default" },
                    arany::Effort::as_str
                )
            )
        } else {
            String::new()
        };
        let id = model_id_preview(
            &item.id,
            usize::from(area.width).saturating_sub(prefix + suffix.len()),
        );
        frame.render_widget(
            Paragraph::new(numbered_line(
                index,
                &format!("{id}{suffix}"),
                armed.then_some(selected),
                area.width,
            )),
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
    for (offset, text) in [(2, navigation), (1, action)].into_iter().filter(|_| armed) {
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
        || row >= area.bottom().saturating_sub(5)
    {
        return None;
    }
    let visible_rows = usize::from(area.height.saturating_sub(6));
    let offset = usize::from(row - area.top() - 1);
    let index = session_picker_start(selected, visible_rows).saturating_add(offset);
    (index < count).then_some(index)
}

pub(super) fn agent_detail_lines(model: &AgentInspectorModel, width: u16) -> Vec<Line<'static>> {
    model
        .lines
        .iter()
        .skip(1)
        .flat_map(|line| {
            if line.starts_with("Supervisor (shadow):") {
                wrap_cells(line, usize::from(width), 0)
                    .into_iter()
                    .map(|row| Line::from(row).style(Style::default().add_modifier(Modifier::DIM)))
                    .collect::<Vec<_>>()
            } else {
                vec![Line::from(safe_truncate(line, usize::from(width)))]
            }
        })
        .collect()
}

pub(super) const AGENT_PICKER_ROWS: u16 = 23;

// The Session line, the search line, a window of rows, the focused row's details and the key hint.
pub(super) fn draw_agent_inspector_frame(
    frame: &mut Frame,
    area: Rect,
    model: &AgentInspectorModel,
    scroll: usize,
    armed: bool,
    notice: Option<&str>,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(model.lines[0].as_str()).style(palette.selection()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(model.search.as_str()),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    let choices = agent_choice_rows(area.height, model.choices.len());
    let start = super::agents::window_start(model.selected, choices);
    for (index, choice) in model.choices.iter().enumerate().skip(start).take(choices) {
        frame.render_widget(
            Paragraph::new(numbered_line(
                index,
                choice,
                model.selected.filter(|_| armed),
                area.width,
            )),
            Rect::new(area.x, area.y + 2 + (index - start) as u16, area.width, 1),
        );
    }
    let detail_y = area
        .y
        .saturating_add(2 + u16::try_from(choices).unwrap_or(u16::MAX));
    let detail_rows = agent_detail_rows(area.height, model.choices.len());
    for (index, line) in agent_detail_lines(model, area.width)
        .into_iter()
        .skip(scroll)
        .take(detail_rows)
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, detail_y + index as u16, area.width, 1),
        );
    }
    if armed && area.height > 2 {
        let hint = match area.width {
            70.. => "Enter open · Esc close · Up/Down agent · type to search · PgUp/PgDn",
            40.. => "Enter open · Esc close · type to search",
            20.. => "Type Enter Esc Up/Dn",
            _ => "Esc Up/Dn Pg",
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(
                notice.unwrap_or(hint),
                usize::from(area.width),
            )),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) const AGENT_CHAT_ROWS: u16 = 3;

pub(super) struct AgentChatFrame<'a> {
    pub label: &'a str,
    pub draft: Option<&'a Composer>,
    pub editable: bool,
}

impl<'a> From<&'a str> for AgentChatFrame<'a> {
    fn from(label: &'a str) -> Self {
        Self {
            label,
            draft: None,
            editable: false,
        }
    }
}

// The actor label, its composer and, once armed, the key hint.
pub(super) fn draw_agent_chat_frame(
    frame: &mut Frame,
    area: Rect,
    state: AgentChatFrame<'_>,
    armed: bool,
    notice: Option<&str>,
    fullscreen: bool,
    palette: Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let width = usize::from(area.width);
    let label = state.label;
    let mode = if state.editable {
        "message"
    } else {
        "read-only"
    };
    let full = format!("viewing {label} ({mode})");
    let short = format!("{label} ({mode})");
    let title = [full.as_str(), short.as_str()]
        .into_iter()
        .find(|title| UnicodeWidthStr::width(*title) <= width)
        .map_or_else(
            || safe_truncate(&format!("{label} {mode}"), width),
            str::to_owned,
        );
    let composer = if state.editable {
        "> Message · Enter offers guidance"
    } else {
        match width {
            40.. => "> Read-only · Esc returns to primary",
            20.. => "> Read-only · Esc",
            _ => "> Read-only",
        }
    };
    let hint = match (width, fullscreen) {
        (60.., true) => "Alt+Left/Right agents · Esc primary · PgUp/PgDn scroll",
        (40.., _) => "Alt+Left/Right agents · Esc primary",
        (24.., _) => "Alt+←/→ agents · Esc",
        _ => "Esc primary",
    };
    let draft_rows = state
        .draft
        .map(|draft| wrapped_composer(draft, width.saturating_sub(2)));
    let draft_line = draft_rows
        .as_ref()
        .filter(|_| state.draft.is_some_and(|draft| !draft.is_empty()))
        .map(|rows| format!("> {}", rows.lines[rows.cursor_row]));
    let rows = [
        Some((title, palette.selection())),
        Some((
            safe_truncate(draft_line.as_deref().unwrap_or(composer), width),
            palette.subtle(),
        )),
        armed.then(|| {
            (
                safe_truncate(notice.unwrap_or(hint), width),
                Style::default(),
            )
        }),
    ];
    for (offset, row) in rows.into_iter().enumerate().take(usize::from(area.height)) {
        if let Some((text, style)) = row {
            frame.render_widget(
                Paragraph::new(text).style(style),
                Rect::new(area.x, area.y + offset as u16, area.width, 1),
            );
        }
    }
    if state.editable
        && armed
        && area.height >= 2
        && area.width >= 3
        && let Some(rows) = draft_rows
    {
        frame.set_cursor_position(Position {
            x: area.x
                + 2
                + u16::try_from(rows.cursor_cells)
                    .unwrap_or(u16::MAX)
                    .min(area.width - 3),
            y: area.y + 1,
        });
    }
}

pub(super) fn agent_choice_rows(height: u16, count: usize) -> usize {
    usize::from(height.saturating_sub(3))
        .min(crate::presentation::MAX_AGENT_ROWS)
        .min(count)
}

pub(super) fn agent_detail_rows(height: u16, count: usize) -> usize {
    usize::from(height.saturating_sub(3)).saturating_sub(agent_choice_rows(height, count))
}

pub(super) fn agent_picker_index(
    area: Rect,
    column: u16,
    row: u16,
    selected: Option<usize>,
    count: usize,
) -> Option<usize> {
    let visible = agent_choice_rows(area.height, count);
    let first = area.top().saturating_add(2);
    if column < area.left()
        || column >= area.right()
        || row < first
        || usize::from(row - first) >= visible
    {
        return None;
    }
    let index = super::agents::window_start(selected, visible) + usize::from(row - first);
    (index < count).then_some(index)
}

#[cfg(test)]
mod tests;
