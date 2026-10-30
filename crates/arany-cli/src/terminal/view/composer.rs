use super::*;

const MAX_VISIBLE_DRAFT_ROWS: usize = 4;

pub(super) struct ComposerRows {
    pub(super) lines: Vec<String>,
    pub(super) cursor_row: usize,
    pub(super) cursor_cells: usize,
}

pub(in crate::terminal) fn paste_detail_scroll_limit(
    text: &str,
    width: usize,
    height: usize,
) -> usize {
    text.split('\n')
        .map(|line| wrap_cells(&crate::presentation::escape_terminal(line), width.max(1), 0).len())
        .sum::<usize>()
        .saturating_sub(height.saturating_sub(2))
}

pub(in crate::terminal) fn draw_paste_detail(
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

pub(super) fn wrapped_composer(composer: &Composer, width: usize) -> ComposerRows {
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

pub(in crate::terminal) fn composer_height(composer: &Composer, area: Rect) -> u16 {
    let rows = wrapped_composer(composer, usize::from(area.width.saturating_sub(2)));
    (rows.lines.len().min(MAX_VISIBLE_DRAFT_ROWS) as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(1))
}

#[derive(Clone, Copy)]
pub(in crate::terminal) struct ComposerHints<'a> {
    pub(super) palette: Palette,
    pub(super) menu_height: u16,
    pub(super) picker_focused: bool,
    pub(super) newline: &'a str,
}

impl<'a> ComposerHints<'a> {
    pub(in crate::terminal) fn new(palette: Palette, newline: &'a str) -> Self {
        Self {
            palette,
            menu_height: 0,
            picker_focused: false,
            newline,
        }
    }
}

pub(super) fn draw_composer(
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
        if composer_area.width >= FULL_LAYOUT_WIDTH {
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
                if composer_area.width < FULL_LAYOUT_WIDTH {
                    "Message or Enter"
                } else {
                    "Add a message, or Enter to send image"
                }
            } else if model.setup_required {
                if composer_area.width < FULL_LAYOUT_WIDTH {
                    "Type /setup"
                } else {
                    "Type /setup to connect an account"
                }
            } else if composer_area.width < FULL_LAYOUT_WIDTH {
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
            if composer_area.width < FULL_LAYOUT_WIDTH {
                " Draft kept "
            } else {
                " Draft kept · Esc returns "
            }
        } else if model.draft_action == DraftAction::Retain
            && composer_area.width >= FULL_LAYOUT_WIDTH
        {
            "Enter keeps draft · \\ then Enter newline"
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 24 {
            " Enter keeps · \\ Enter "
        } else if model.draft_action == DraftAction::Retain && composer_area.width >= 16 {
            " Enter keeps "
        } else if menu_height > 0 && composer_area.width >= FULL_LAYOUT_WIDTH {
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
        } else if command && composer_area.width >= FULL_LAYOUT_WIDTH {
            " Enter command · \\ then Enter newline "
        } else if command && composer_area.width >= 16 {
            " Enter command "
        } else if model.draft_action != DraftAction::Submit
            && composer_area.width >= FULL_LAYOUT_WIDTH
        {
            "Enter keeps draft · \\ then Enter newline"
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 24 {
            " Enter keeps · \\ Enter "
        } else if model.draft_action != DraftAction::Submit && composer_area.width >= 16 {
            " Enter keeps "
        } else if composer_area.width >= FULL_LAYOUT_WIDTH {
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
            FULL_LAYOUT_WIDTH.. if agents_hint => " Alt+Left/Right agents ",
            24.. if agents_hint => " Alt+←/→ agents ",
            _ => hint,
        };
        let hint = if composer_area.width >= FULL_LAYOUT_WIDTH && newline != "\\ then Enter" {
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
