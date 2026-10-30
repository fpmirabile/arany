use super::*;

pub(in crate::terminal) fn agent_detail_lines(
    model: &AgentInspectorModel,
    width: u16,
) -> Vec<Line<'static>> {
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

pub(in crate::terminal) const AGENT_PICKER_ROWS: u16 = 23;

// The Session line, the search line, a window of rows, the focused row's details and the key hint.
pub(in crate::terminal) struct AgentInspectorFrame<'a> {
    pub model: &'a AgentInspectorModel,
    pub scroll: usize,
    pub armed: bool,
    pub notice: Option<&'a str>,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_agent_inspector_frame(
    frame: &mut Frame,
    area: Rect,
    state: AgentInspectorFrame<'_>,
) {
    let AgentInspectorFrame {
        model,
        scroll,
        armed,
        notice,
        palette,
    } = state;
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
    let start = window_start(model.selected, choices);
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
            FULL_LAYOUT_WIDTH.. => "Enter open · Esc close · type to search",
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

pub(in crate::terminal) const AGENT_CHAT_ROWS: u16 = 3;

pub(in crate::terminal) struct AgentChatFrame<'a> {
    pub label: &'a str,
    pub draft: Option<&'a Composer>,
    pub editable: bool,
    pub armed: bool,
    pub notice: Option<&'a str>,
    pub fullscreen: bool,
    pub palette: Palette,
}

// The actor label, its composer and, once armed, the key hint.
pub(in crate::terminal) fn draw_agent_chat_frame(
    frame: &mut Frame,
    area: Rect,
    state: AgentChatFrame<'_>,
) {
    let (armed, notice, fullscreen, palette) =
        (state.armed, state.notice, state.fullscreen, state.palette);
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
        match area.width {
            FULL_LAYOUT_WIDTH.. => "> Read-only · Esc returns to primary",
            20.. => "> Read-only · Esc",
            _ => "> Read-only",
        }
    };
    let hint = match (area.width, fullscreen) {
        (60.., true) => "Alt+Left/Right agents · Esc primary · PgUp/PgDn scroll",
        (FULL_LAYOUT_WIDTH.., _) => "Alt+Left/Right agents · Esc primary",
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

pub(in crate::terminal) fn agent_choice_rows(height: u16, count: usize) -> usize {
    usize::from(height.saturating_sub(3))
        .min(crate::presentation::MAX_AGENT_ROWS)
        .min(count)
}

pub(in crate::terminal) fn agent_detail_rows(height: u16, count: usize) -> usize {
    usize::from(height.saturating_sub(3)).saturating_sub(agent_choice_rows(height, count))
}

pub(in crate::terminal) fn agent_picker_index(
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
    let index = window_start(selected, visible) + usize::from(row - first);
    (index < count).then_some(index)
}
