use super::*;

pub(super) fn setup_area(full: Rect) -> Rect {
    let height = full.height.min(if full.width < 20 { 8 } else { 7 });
    let width = full.width.min(64);
    Rect::new(
        full.x.saturating_add((full.width - width) / 2),
        full.y + full.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(in crate::terminal) struct SetupFrame<'a> {
    pub step: u8,
    pub title: &'a str,
    pub instruction: &'a str,
    pub input_chars: usize,
    pub notice: Option<&'a str>,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_setup_frame(frame: &mut Frame, full: Rect, state: SetupFrame<'_>) {
    let SetupFrame {
        step,
        title,
        instruction,
        input_chars,
        notice,
        palette,
    } = state;
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

pub(in crate::terminal) struct SetupTextFrame<'a> {
    pub title: &'a str,
    pub instruction: &'a str,
    pub draft: &'a Composer,
    pub notice: Option<&'a str>,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_setup_text_frame(
    frame: &mut Frame,
    full: Rect,
    state: SetupTextFrame<'_>,
) {
    let SetupTextFrame {
        title,
        instruction,
        draft,
        notice,
        palette,
    } = state;
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
        Paragraph::new(if area.width < FULL_LAYOUT_WIDTH {
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

const SETUP_CHOICES_CHROME_ROWS: u16 = 4;

pub(in crate::terminal) fn setup_choices_height(choices: usize) -> u16 {
    u16::try_from(choices)
        .unwrap_or(u16::MAX)
        .saturating_add(SETUP_CHOICES_CHROME_ROWS)
        .max(MIN_DIALOG_HEIGHT)
}

pub(in crate::terminal) struct SetupChoicesFrame<'a> {
    pub title: &'a str,
    pub instruction: &'a str,
    pub choices: &'a [(u8, &'a str)],
    pub selected: usize,
    pub notice: Option<&'a str>,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_setup_choices_frame(
    frame: &mut Frame,
    full: Rect,
    state: SetupChoicesFrame<'_>,
) {
    let SetupChoicesFrame {
        title,
        instruction,
        choices,
        selected,
        notice,
        palette,
    } = state;
    let height = full.height.min(setup_choices_height(choices.len()));
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
        let row = Rect::new(area.x, area.y + 2 + (offset - first) as u16, area.width, 1);
        draw_setup_choice(frame, row, label, offset == selected, palette);
    }
    if !dense {
        draw_setup_instruction(frame, area, instruction, row_count);
    }
    if !dense || notice.is_some() {
        let footer = notice.unwrap_or(if compact {
            "↑↓ Enter Esc"
        } else {
            "Up/Down · Enter selects · Esc closes"
        });
        let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        draw_setup_notice(frame, row, footer, notice.is_some(), palette);
    }
}

// The instruction wraps below the choices, above the footer row.
fn draw_setup_instruction(frame: &mut Frame, area: Rect, instruction: String, row_count: usize) {
    let instruction_y = area.y + 2 + row_count as u16;
    let instruction_height = area.bottom().saturating_sub(instruction_y + 1);
    if instruction_height > 0 {
        frame.render_widget(
            Paragraph::new(instruction).wrap(Wrap { trim: false }),
            Rect::new(area.x, instruction_y, area.width, instruction_height),
        );
    }
}

fn draw_setup_choice(
    frame: &mut Frame,
    row: Rect,
    label: &str,
    highlighted: bool,
    palette: Palette,
) {
    let line = format!("{} {label}", if highlighted { '>' } else { ' ' });
    frame.render_widget(
        Paragraph::new(safe_truncate(&line, usize::from(row.width))).style(if highlighted {
            palette.selection()
        } else {
            Style::default()
        }),
        row,
    );
}

// A notice takes the footer row in the attention color.
fn draw_setup_notice(frame: &mut Frame, row: Rect, text: &str, notice: bool, palette: Palette) {
    frame.render_widget(
        Paragraph::new(safe_truncate(text, usize::from(row.width))).style(if notice {
            palette.attention()
        } else {
            Style::default()
        }),
        row,
    );
}

pub(in crate::terminal) fn setup_warning_height(warning: &str, width: u16) -> u16 {
    u16::try_from(
        wrap_setup_warning(warning, usize::from(width.clamp(1, 72)))
            .len()
            .saturating_add(5),
    )
    .unwrap_or(u16::MAX)
    .clamp(MIN_DIALOG_HEIGHT, 16)
}

pub(in crate::terminal) struct SetupWarningFrame<'a> {
    pub title: &'a str,
    pub warning: &'a str,
    pub page: usize,
    pub accept_selected: bool,
    pub notice: Option<&'a str>,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_setup_warning_frame(
    frame: &mut Frame,
    full: Rect,
    state: SetupWarningFrame<'_>,
) -> bool {
    let SetupWarningFrame {
        title,
        warning,
        page,
        accept_selected,
        notice,
        palette,
    } = state;
    let width = full.width.min(72);
    let height = full.height.min(16);
    if width == 0 || height < MIN_DIALOG_HEIGHT {
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
    if has_more {
        let prompt = if width <= 24 {
            format!("Page {}/{} Enter", page + 1, pages)
        } else {
            format!("Page {}/{} · PgDn/Enter", page + 1, pages)
        };
        frame.render_widget(
            Paragraph::new(safe_truncate(&prompt, usize::from(width))),
            Rect::new(area.x, area.bottom() - 4, width, 1),
        );
    } else {
        let choices = Rect::new(area.x, area.bottom() - 4, width, 2);
        draw_warning_choices(frame, choices, accept_selected, palette);
    }
    frame.render_widget(
        Paragraph::new(if has_more {
            "Read all pages"
        } else {
            "Tab/Enter"
        }),
        Rect::new(area.x, area.bottom() - 2, width, 1),
    );
    let row = Rect::new(area.x, area.bottom() - 1, width, 1);
    draw_setup_notice(
        frame,
        row,
        notice.unwrap_or("Esc cancels"),
        notice.is_some(),
        palette,
    );
    has_more
}

fn draw_warning_choices(frame: &mut Frame, area: Rect, accept_selected: bool, palette: Palette) {
    for (index, label) in ["Back", "Accept"].iter().enumerate() {
        let focused = accept_selected == (index == 1);
        frame.render_widget(
            Paragraph::new(format!("{} {label}", focus_marker(focused))).style(if focused {
                palette.selection()
            } else {
                Style::default()
            }),
            Rect::new(area.x, area.y + index as u16, area.width, 1),
        );
    }
}

pub(super) fn wrap_setup_warning(warning: &str, width: usize) -> Vec<String> {
    let normalized = warning.split_whitespace().collect::<Vec<_>>().join(" ");
    wrap_cells(&normalized, width, 0)
}
