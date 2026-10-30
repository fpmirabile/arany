use super::*;

pub(in crate::terminal) fn draw_session_picker_frame(
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
    let start = window_start(Some(selected), rows);
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
        let short = area.width < FULL_LAYOUT_WIDTH;
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
        } else if area.width < FULL_LAYOUT_WIDTH {
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

pub(in crate::terminal) fn draw_quick_actions_frame(
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
    } else if area.width < FULL_LAYOUT_WIDTH {
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
    let first = window_start(Some(selected), visible).min(labels.len().saturating_sub(visible));
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
            } else if area.width < FULL_LAYOUT_WIDTH {
                "Submit draft · Esc back"
            } else {
                "Submit or clear draft · Esc return"
            }
        } else if area.width < 20 {
            "1-5 Enter Esc"
        } else if area.width < FULL_LAYOUT_WIDTH {
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

pub(in crate::terminal) fn settings_height(rows: &[String], width: u16, max_rows: usize) -> u16 {
    let lines_per_row = if settings_stacked(rows, usize::from(width)) {
        2
    } else {
        1
    };
    u16::try_from(rows.len().min(max_rows) * lines_per_row + 2).unwrap_or(u16::MAX)
}

pub(in crate::terminal) fn guidance_height(text: &str, width: u16) -> u16 {
    u16::try_from(wrap_cells(text, usize::from(width.max(1)), 0).len()).unwrap_or(u16::MAX)
}

#[expect(
    clippy::too_many_arguments,
    reason = "a dialog frame takes its title, rows, focus and notice together"
)]
pub(in crate::terminal) fn draw_settings_frame(
    frame: &mut Frame,
    area: Rect,
    title: &str,
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
        Paragraph::new(title.to_owned()).style(palette.selection()),
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
    let start = window_start(Some(selected), visible);
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
        // The widest footer that fits keeps Esc visible.
        let footer = SETTINGS_FOOTERS
            .into_iter()
            .find(|footer| footer.width() <= usize::from(area.width))
            .unwrap_or(SETTINGS_FOOTERS[SETTINGS_FOOTERS.len() - 1]);
        frame.render_widget(
            Paragraph::new(safe_truncate(footer, usize::from(area.width))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

pub(super) const SETTINGS_FOOTERS: [&str; 3] = [
    "1-9 or ↑↓ select · ←→ change · Del default · Enter saves · Esc",
    "↑↓ ←→ Del Enter Esc",
    "↑↓ ←→ Enter Esc",
];

pub(in crate::terminal) fn usage_lines(page: &str, width: u16) -> Vec<String> {
    page.lines()
        .flat_map(|line| wrap_cells(line, usize::from(width).max(1), 0))
        .collect()
}

pub(in crate::terminal) fn draw_usage_frame(
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
        let hint = if area.width < FULL_LAYOUT_WIDTH {
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

pub(in crate::terminal) fn draw_help_frame(
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
    let start = window_start(Some(selected), visible);
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

pub(in crate::terminal) fn session_picker_index(
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
    let index = window_start(Some(selected), visible_rows).saturating_add(offset);
    (index < count).then_some(index)
}
