use super::*;

pub(in crate::terminal) struct PermissionFrameState<'a> {
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

pub(in crate::terminal) struct PermissionFrameProgress {
    pub more: bool,
    pub page: usize,
}

const PROFILE_HANGING_INDENT: usize = 6;

pub(in crate::terminal) fn draw_permission_frame(
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
    let (gutter, width) = permission_layout(full.width, boxed);
    if width == 0 || full.height < MIN_DIALOG_HEIGHT {
        return PermissionFrameProgress { more: true, page };
    }
    let mut rows = Vec::new();
    let mut profile_rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let row_start = rows.len();
        let focused = profile_start
            .zip(focused_profile)
            .is_some_and(|(start, selected)| index == start + selected);
        let profile_row = profile_line(index, profile_start, profile_count);
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
            let label = format!("{} {choice}", focus_marker(focused));
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

pub(in crate::terminal) fn tool_approval_height(
    lines: &[String],
    choices: &[crate::presentation::ToolApprovalChoice],
    width: u16,
) -> u16 {
    let width = usize::from(
        width
            .saturating_sub(if width >= FULL_LAYOUT_WIDTH { 4 } else { 0 })
            .max(1),
    );
    let rows = lines
        .iter()
        .map(|line| wrap_cells(line, width, 0).len())
        .sum::<usize>();
    let chrome =
        tool_approval_controls(choices, width, None).len() + 1 + tool_approval_hint_height(width);
    u16::try_from(rows.saturating_add(chrome))
        .unwrap_or(u16::MAX)
        .max(MIN_DIALOG_HEIGHT)
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

pub(in crate::terminal) struct ToolApprovalFrameState {
    pub page: usize,
    pub focus: Option<usize>,
    pub review_notice: bool,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_tool_approval_frame(
    frame: &mut Frame,
    area: Rect,
    lines: &[String],
    choices: &[crate::presentation::ToolApprovalChoice],
    state: ToolApprovalFrameState,
) -> bool {
    let ToolApprovalFrameState {
        page,
        focus,
        review_notice,
        palette,
    } = state;
    frame.render_widget(Clear, area);
    let gutter = u16::from(area.width >= FULL_LAYOUT_WIDTH) * 2;
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
        .enumerate()
        .flat_map(|(index, line)| {
            let style = if index == 0 && line == crate::presentation::HOST_SENSITIVE_PATH_WARNING {
                palette.attention()
            } else if line == crate::presentation::HOST_SESSION_APPROVAL_HINT {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            wrap_cells(line, width, 0)
                .into_iter()
                .map(move |row| Line::styled(row, style))
        })
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

// The side gutter and text width of a permission dialog drawn across `width` columns.
fn permission_layout(width: u16, boxed: bool) -> (u16, u16) {
    let gutter = if width >= FULL_LAYOUT_WIDTH { 2 } else { 0 };
    let text_width = width.saturating_sub(gutter * 2);
    if boxed {
        (0, text_width.min(76))
    } else {
        (gutter, text_width)
    }
}

fn profile_line(index: usize, profile_start: Option<usize>, profile_count: usize) -> bool {
    profile_start.is_some_and(|start| (start..start + profile_count).contains(&index))
}

pub(in crate::terminal) fn permission_height(
    lines: &[String],
    width: u16,
    boxed: bool,
    profile_start: Option<usize>,
    profile_count: usize,
) -> u16 {
    let (_, width) = permission_layout(width, boxed);
    let rows = lines
        .iter()
        .enumerate()
        .fold(0usize, |rows, (index, line)| {
            let indent = if profile_line(index, profile_start, profile_count) {
                PROFILE_HANGING_INDENT
            } else {
                0
            };
            rows.saturating_add(wrap_cells(line, usize::from(width.max(1)), indent).len())
        });
    let height = u16::try_from(rows.saturating_add(5))
        .unwrap_or(u16::MAX)
        .max(MIN_DIALOG_HEIGHT);
    if boxed { height.min(20) } else { height }
}
