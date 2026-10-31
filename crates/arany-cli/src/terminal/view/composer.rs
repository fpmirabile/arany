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

pub(in crate::terminal) struct PasteDetailFrame<'a> {
    pub label: &'a str,
    pub text: &'a str,
    pub scroll: usize,
    pub palette: Palette,
}

pub(in crate::terminal) fn draw_paste_detail(
    frame: &mut Frame,
    area: Rect,
    state: PasteDetailFrame<'_>,
) {
    let PasteDetailFrame {
        label,
        text,
        scroll,
        palette,
    } = state;
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
        picker_focused,
        ..
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
    let cursor_line = draft_rows.lines[draft_rows.cursor_row].as_str();
    let ghost = ghost_span(
        preview.as_ref(),
        inner_width.saturating_sub(UnicodeWidthStr::width(cursor_line)),
        palette,
    );
    let mut rendered_lines = draft_lines(
        composer,
        &draft_rows,
        first_line..first_line + visible_lines,
        ghost,
        palette,
    );
    let position = draft_position(&draft_rows, first_line, visible_lines, area.width);
    let (compact_image_chip, position) = image_heading(composer, position, area.width);
    let title = if compact_image_chip {
        ""
    } else if 2 + "Ask Arany".len() + position.len() <= usize::from(composer_area.width) {
        "Ask Arany"
    } else {
        "Draft"
    };
    let image_chips = image_chip_areas(composer, area, title, compact_image_chip);
    draw_heading(frame, area, title, position, palette);
    if visible_lines > 0 && composer_area.width >= 3 {
        if draft.is_empty() {
            let hint = empty_draft_hint(composer, model, composer_area.width);
            rendered_lines[0] = Line::styled(safe_truncate(hint, inner_width), palette.subtle());
        }
        frame.render_widget(
            Paragraph::new(Line::styled("> ", palette.accent())),
            Rect::new(composer_area.x, composer_area.y + 1, 2, 1),
        );
        let hint = footer_hint(composer, model, hints, composer_area.width);
        draw_footer(frame, composer_area, hint, palette);
        frame.render_widget(
            Paragraph::new(rendered_lines),
            Rect::new(
                composer_area.x + 2,
                composer_area.y + 1,
                composer_area.width - 2,
                u16::try_from(visible_lines).unwrap_or(u16::MAX),
            ),
        );
        place_cursor(frame, composer_area, &draft_rows, first_line);
    }
    image_chips
}

fn draw_footer(frame: &mut Frame, area: Rect, hint: String, palette: Palette) {
    let rules = usize::from(area.width).saturating_sub(UnicodeWidthStr::width(hint.as_str()));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("─".repeat(rules / 2), palette.subtle()),
            Span::raw(hint),
            Span::styled("─".repeat(rules - rules / 2), palette.subtle()),
        ])),
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
    );
}

fn place_cursor(frame: &mut Frame, area: Rect, draft_rows: &ComposerRows, first_line: usize) {
    let cursor_x = area
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(draft_rows.cursor_cells).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(1));
    frame.set_cursor_position(Position {
        x: cursor_x,
        y: area
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(draft_rows.cursor_row - first_line).unwrap_or(u16::MAX)),
    });
}

// The command preview's ghost text, bracketed when color cannot dim it.
fn ghost_span(
    preview: Option<&super::super::commands::CommandPreview>,
    available: usize,
    palette: Palette,
) -> Span<'static> {
    let ghost = preview.map_or(String::new(), |preview| {
        if palette.color || preview.ghost.is_empty() || preview.ghost_is_placeholder {
            safe_truncate(&preview.ghost, available)
        } else if available >= 3 {
            format!("[{}]", safe_truncate(&preview.ghost, available - 2))
        } else {
            String::new()
        }
    });
    Span::styled(
        ghost,
        if palette.color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        },
    )
}

fn draft_lines(
    composer: &Composer,
    draft_rows: &ComposerRows,
    window: std::ops::Range<usize>,
    ghost: Span<'static>,
    palette: Palette,
) -> Vec<Line<'static>> {
    let mut rendered_lines = Vec::with_capacity(window.len());
    let mut slash_cells = composer
        .slash_token()
        .map_or(0, |token| token.text.len())
        .saturating_sub(
            draft_rows.lines[..window.start]
                .iter()
                .map(String::len)
                .sum(),
        );
    let mut ghost = Some(ghost);
    for (index, line) in draft_rows
        .lines
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.len())
    {
        let mut spans = slash_spans(
            line.clone(),
            slash_cells.min(line.len()),
            Style::default(),
            palette,
        );
        slash_cells = slash_cells.saturating_sub(line.len());
        if index == draft_rows.cursor_row
            && let Some(ghost) = ghost.take()
        {
            spans.push(ghost);
        }
        rendered_lines.push(Line::from(spans));
    }
    rendered_lines
}

fn draft_position(
    draft_rows: &ComposerRows,
    first_line: usize,
    visible_lines: usize,
    width: u16,
) -> String {
    if visible_lines > 0 && draft_rows.lines.len() > visible_lines {
        if width >= FULL_LAYOUT_WIDTH {
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
    }
}

// Image chips join the row position in the heading, collapsing to one chip when they do not fit.
fn image_heading(composer: &Composer, mut position: String, width: u16) -> (bool, String) {
    let mut compact_image_chip = !composer.images().is_empty() && width < 28;
    if composer.images().is_empty() {
        return (compact_image_chip, position);
    }
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
    compact_image_chip |=
        UnicodeWidthStr::width(all_chips.as_str()) > usize::from(width).saturating_sub(12);
    if compact_image_chip {
        let chip = match (composer.selected_image(), composer.images().len()) {
            (Some(index), _) => format!(" >[Image #{}]", index + 1),
            (None, 1) => " [Image #1]".to_owned(),
            (None, count) => format!(" [{count} images]"),
        };
        position = if UnicodeWidthStr::width(chip.as_str())
            + UnicodeWidthStr::width(position.as_str())
            <= usize::from(width)
        {
            format!("{chip}{position}")
        } else {
            chip
        };
    } else {
        position = all_chips;
    }
    (compact_image_chip, position)
}

fn image_chip_areas(
    composer: &Composer,
    area: Rect,
    title: &str,
    compact_image_chip: bool,
) -> Vec<(usize, Rect)> {
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
    image_chips
}

fn draw_heading(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    position: String,
    palette: Palette,
) {
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
                    "─".repeat(usize::from(area.width).saturating_sub(heading_cells))
                ),
                palette.subtle(),
            ),
        ])),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

fn empty_draft_hint(composer: &Composer, model: &PresentationModel, width: u16) -> &'static str {
    if !composer.images().is_empty() {
        if width < FULL_LAYOUT_WIDTH {
            "Message or Enter"
        } else {
            "Add a message, or Enter to send image"
        }
    } else if model.setup_required {
        if width < FULL_LAYOUT_WIDTH {
            "Type /setup"
        } else {
            "Type /setup to connect an account"
        }
    } else if width < FULL_LAYOUT_WIDTH {
        "Message Arany"
    } else {
        "Ask a question, or type / for commands"
    }
}

fn footer_hint(
    composer: &Composer,
    model: &PresentationModel,
    hints: ComposerHints<'_>,
    width: u16,
) -> String {
    let ComposerHints {
        menu_height,
        picker_focused,
        newline,
        ..
    } = hints;
    let draft = composer.text();
    let agents_hint = draft.is_empty()
        && composer.images().is_empty()
        && model.agent_switch
        && menu_height == 0
        && !picker_focused;
    let hint = match width {
        FULL_LAYOUT_WIDTH.. if agents_hint => " Alt+Left/Right agents ",
        24.. if agents_hint => " Alt+←/→ agents ",
        _ => key_hint(composer, model, hints, width),
    };
    if width >= FULL_LAYOUT_WIDTH && newline != "\\ then Enter" {
        hint.replace("\\ then Enter newline", &format!("{} / \\ Enter", newline))
            .replace("Enter keeps draft", "Enter keeps")
    } else {
        hint.to_owned()
    }
}

fn key_hint(
    composer: &Composer,
    model: &PresentationModel,
    hints: ComposerHints<'_>,
    width: u16,
) -> &'static str {
    let draft = composer.text();
    let (menu_height, picker_focused) = (hints.menu_height, hints.picker_focused);
    let command = model.draft_action != DraftAction::Retain
        && matches!(composer.submission(), Ok(Submission::Command { .. }));
    if composer.selected_image().is_some() {
        let image_hint = " ←/→ select · Del/Backspace remove ";
        if usize::from(width) >= UnicodeWidthStr::width(image_hint) {
            image_hint
        } else {
            " Del remove "
        }
    } else if composer.selected_paste().is_some() {
        if width >= 28 {
            " Tab: inspect · Del: remove "
        } else {
            " Tab / Del "
        }
    } else if picker_focused {
        if width < FULL_LAYOUT_WIDTH {
            " Draft kept "
        } else {
            " Draft kept · Esc returns "
        }
    } else if model.draft_action == DraftAction::Retain && width >= FULL_LAYOUT_WIDTH {
        "Enter keeps draft · \\ then Enter newline"
    } else if model.draft_action == DraftAction::Retain && width >= 24 {
        " Enter keeps · \\ Enter "
    } else if model.draft_action == DraftAction::Retain && width >= 16 {
        " Enter keeps "
    } else if menu_height > 0 {
        menu_hint(draft.starts_with('/'), width)
    } else if command && width >= FULL_LAYOUT_WIDTH {
        " Enter command · \\ then Enter newline "
    } else if command && width >= 16 {
        " Enter command "
    } else if model.draft_action != DraftAction::Submit && width >= FULL_LAYOUT_WIDTH {
        "Enter keeps draft · \\ then Enter newline"
    } else if model.draft_action != DraftAction::Submit && width >= 24 {
        " Enter keeps · \\ Enter "
    } else if model.draft_action != DraftAction::Submit && width >= 16 {
        " Enter keeps "
    } else if width >= FULL_LAYOUT_WIDTH {
        " Enter sends · \\ then Enter newline "
    } else if width >= 16 {
        " Enter · \\ Enter "
    } else {
        ""
    }
}

fn menu_hint(slash: bool, width: u16) -> &'static str {
    match (width, slash) {
        (FULL_LAYOUT_WIDTH.., true) => " Tab completes · Enter invokes · Esc close ",
        (FULL_LAYOUT_WIDTH.., false) => " Up/Down · Tab/Enter fills · Esc close ",
        (20.., true) => " Tab fills Enter runs ",
        (20.., false) => " Tab/Enter fills Esc ",
        (_, true) => " Enter runs Esc ",
        (_, false) => " Tab/Enter Esc ",
    }
}
