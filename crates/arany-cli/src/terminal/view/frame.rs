use super::*;

// Rows of the composer frame from the bottom: status, composer, completion menu, activity and history.
pub(in crate::terminal) struct FrameLayout {
    pub composer: u16,
    pub menu: u16,
    pub activity: u16,
    pub history_padding: u16,
    pub history: u16,
}

pub(in crate::terminal) fn frame_layout(
    composer: &Composer,
    area: Rect,
    activity_lines: usize,
) -> FrameLayout {
    let composer_rows = composer_height(composer, area);
    let above_composer = area.height.saturating_sub(1 + composer_rows);
    let menu = completion_height(composer, above_composer);
    let above_menu = above_composer - menu;
    let activity = above_menu.min(u16::try_from(activity_lines).unwrap_or(u16::MAX));
    let available_history = above_menu - activity;
    let history_padding = history_top_padding(available_history);
    FrameLayout {
        composer: composer_rows,
        menu,
        activity,
        history_padding,
        history: available_history - history_padding,
    }
}

pub(in crate::terminal) fn classic_height(
    model: &PresentationModel,
    composer: &Composer,
    screen: Rect,
    live_rows: usize,
) -> u16 {
    let FrameLayout {
        composer: draft,
        menu,
        ..
    } = frame_layout(composer, screen, 0);
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

pub(in crate::terminal) fn history_top_padding(available_height: u16) -> u16 {
    u16::from(available_height > 4)
}

pub(in crate::terminal) fn completion_height(composer: &Composer, available_height: u16) -> u16 {
    composer.completion_rows().map_or(0, |(names, _)| {
        let height = if composer.completion_header().is_some() {
            names.len().clamp(1, 20) as u16 + 1
        } else {
            names.len().min(3) as u16
        };
        height.min(available_height.saturating_sub(1))
    })
}

#[cfg(test)]
pub(in crate::terminal) fn draw_frame(
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

#[cfg(test)]
pub(in crate::terminal) fn draw_frame_with_history(
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

pub(in crate::terminal) struct FrameGeometry {
    pub history_area: Rect,
    pub image_chips: Vec<(usize, Rect)>,
}

pub(in crate::terminal) fn draw_frame_with_history_and_chips(
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
    let FrameLayout {
        composer: composer_height,
        menu: menu_height,
        activity: activity_rows,
        history_padding,
        history: history_height,
    } = frame_layout(composer, area, model.activity_lines.len());
    let menu_y = area.bottom().saturating_sub(1 + menu_height);
    let composer_y = menu_y.saturating_sub(composer_height);
    let history_top = area.y + history_padding;
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
            let header = if area.width < FULL_LAYOUT_WIDTH {
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
                let commands = if area.width < FULL_LAYOUT_WIDTH {
                    "Cmd"
                } else {
                    "Commands"
                };
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
                    if area.width < FULL_LAYOUT_WIDTH {
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
            let line = if header.is_some() && area.width >= FULL_LAYOUT_WIDTH {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::terminal) enum PreviewInterior {
    Repaint,
    Graphics,
}

pub(in crate::terminal) fn draw_image_preview(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    interior: PreviewInterior,
) {
    frame.render_widget(Clear, area);
    let title = safe_truncate(title, usize::from(area.width.saturating_sub(4)));
    frame.render_widget(Block::bordered().title(title), area);
    if interior == PreviewInterior::Repaint {
        return;
    }
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
